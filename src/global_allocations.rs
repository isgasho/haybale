use crate::backend::{Backend, SolverRef};
use crate::config::FunctionHook;
use llvm_ir::*;
use llvm_ir::module::{GlobalVariable, Linkage};
use log::debug;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::hash::{Hash, Hasher};

/// `GlobalAllocations` is responsible for keeping track of which global variable
/// names in which modules resolve to which addresses.
///
/// It has to take into account both module-private and public definitions, of
/// both the strong and weak varieties.
#[derive(Clone)]
pub(crate) struct GlobalAllocations<'p, B: Backend> {
    /// Map from `Name`s of global variables and `Function`s, to addresses at
    /// which they are allocated. These definitions can be either "strong" or
    /// "weak"; see notes on [`Definition`](enum.Definition.html).
    allocated_globals: HashMap<Name, Definition<B::BV>>,
    /// Map from `FunctionHook`s to addresses at which they are allocated.
    /// Currently, `FunctionHook` definitions are always "strong".
    allocated_hooks: HashMap<FunctionHook<'p, B>, B::BV>,
    /// Somewhat a reverse of the above two maps: this is a map from an address
    /// to the `Callable` which was allocated at that address (if any)
    addr_to_function: HashMap<u64, Callable<'p, B>>,
    /// While `allocated_globals` is for "public" (non-module-private) globals,
    /// this is a similar map for module-private globals.
    /// It maps module names to maps of global names to allocated addresses.
    /// Module-private definitions are always strong; they can never be weak.
    module_private_allocated_globals: HashMap<String, HashMap<Name, B::BV>>,
    /// This is to `module_private_allocated_globals` as `addr_to_function` is
    /// to `allocated_globals`
    module_private_addr_to_function: HashMap<String, HashMap<u64, Callable<'p, B>>>,
}

/// Strong and weak definitions.
///
/// Our definitions of "strong" and "weak" are slightly different than the LLVM
/// ones. In the case of multiple definitions of a single name in the same scope:
///   - Two strong definitions is an error
///   - One strong and one weak definition, the strong definition wins
///   - Two weak definitions, one will be chosen arbitrarily
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Definition<V> {
    Strong(V),
    Weak(V),
}

impl<V> Definition<V> {
    fn get(&self) -> &V {
        match self {
            Definition::Strong(v) => &v,
            Definition::Weak(v) => &v,
        }
    }
}

/// Both LLVM `Function`s and `FunctionHook`s can be assigned addresses, and
/// function pointers can point to either
pub(crate) enum Callable<'p, B: Backend> {
    LLVMFunction(&'p Function),
    FunctionHook(FunctionHook<'p, B>),
}

impl<'p, B: Backend> Clone for Callable<'p, B> {
    fn clone(&self) -> Self {
        match self {
            Callable::LLVMFunction(f) => Callable::LLVMFunction(f),
            Callable::FunctionHook(h) => Callable::FunctionHook(h.clone()),
        }
    }
}

impl<'p, B: Backend> fmt::Debug for Callable<'p, B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Callable::LLVMFunction(func) => {
                write!(f, "<Function {:?}>", &func.name)
            },
            Callable::FunctionHook(_) => write!(f, "<FunctionHook>"),
        }
    }
}

impl<'p, B: Backend> PartialEq for Callable<'p, B> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Callable::LLVMFunction(f1), Callable::LLVMFunction(f2)) => f1.name == f2.name,  // assume functions are unique by name
            (Callable::FunctionHook(f1), Callable::FunctionHook(f2)) => f1 == f2,
            (_, _) => false,
        }
    }
}

// our implementation of `PartialEq` satisfies `Eq` under our assumptions
impl<'p, B: Backend> Eq for Callable<'p, B> {}

impl<'p, B: Backend> Hash for Callable<'p, B> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Callable::LLVMFunction(f) => f.name.hash(state),  // assume functions are unique by name
            Callable::FunctionHook(fh) => fh.hash(state),
        }
    }
}

/// Trait which unifies `GlobalVariable` and `Function`, which are both global objects in LLVM
trait Global {
    fn get_linkage(&self) -> Linkage;
    fn get_name(&self) -> Name;
}

impl Global for GlobalVariable {
    fn get_linkage(&self) -> Linkage {
        self.linkage
    }
    fn get_name(&self) -> Name {
        self.name.clone()
    }
}

impl Global for Function {
    fn get_linkage(&self) -> Linkage {
        self.linkage
    }
    fn get_name(&self) -> Name {
        Name::Name(self.name.clone())
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum AllocationResult {
    /// Allocated the global as public
    Public,
    /// Allocated the global as module-private
    ModulePrivate,
    /// Did not allocate the global (some other definition took precedence)
    NoAllocate,
}

impl<'p, B: Backend> GlobalAllocations<'p, B> {
    pub fn new() -> Self {
        Self {
            allocated_globals: HashMap::new(),
            allocated_hooks: HashMap::new(),
            addr_to_function: HashMap::new(),
            module_private_allocated_globals: HashMap::new(),
            module_private_addr_to_function: HashMap::new(),
        }
    }

    /// `var`: a global variable *definition* (not a declaration)
    ///
    /// `module`: `Module` in which the definition appears
    ///
    /// `addr`: Address at which the global variable should be allocated
    pub fn allocate_global_var(&mut self, var: &'p GlobalVariable, module: &'p Module, addr: B::BV) {
        if var.initializer.is_none() {
            panic!("Can't call allocate_global() with a global declaration, only a definition");
        }
        self.allocate_global(var, module, addr);
    }

    /// `func`: a function definition
    ///
    /// `module`: `Module` in which the definition appears
    ///
    /// `addr`: Address at which the function should be allocated.
    ///
    /// Note that we have to pretend to allocate `Function`s so that we can have
    /// pointers to them. (As of this writing, we actually only allocate 64 bits
    /// for every `Function`)
    pub fn allocate_function(&mut self, func: &'p Function, module: &'p Module, addr: u64, addr_bv: B::BV) {
        match self.allocate_global(func, module, addr_bv) {
            AllocationResult::Public => {
                self.addr_to_function.insert(addr, Callable::LLVMFunction(func));
            },
            AllocationResult::ModulePrivate => {
                self.module_private_addr_to_function
                    .entry(module.name.clone())
                    .or_default()
                    .insert(addr, Callable::LLVMFunction(func));
            },
            AllocationResult::NoAllocate => {},
        }
    }

    /// `hook`: a `FunctionHook`
    ///
    /// `addr`: Address at which the function hook should be allocated.
    /// Pointers with this value will be considered to point to `hook`.
    ///
    /// Note that all function hooks are considered to have global visibility; we
    /// don't at this time support module-private function hooks.
    /// You can still hook module-private functions, but those hooks will apply
    /// to all functions of that name in all modules.
    pub fn allocate_function_hook(&mut self, hook: FunctionHook<'p, B>, addr: u64, addr_bv: B::BV) {
        self.allocated_hooks.insert(hook.clone(), addr_bv);
        self.addr_to_function.insert(addr, Callable::FunctionHook(hook));
    }

    fn allocate_global(&mut self, global: &'p impl Global, module: &'p Module, addr: B::BV) -> AllocationResult {
        match global.get_linkage() {
            Linkage::Private | Linkage::Internal => {
                // Module-private global, strong definition
                debug!("Allocating {:?} (module-private to {:?}) at {:?}", global.get_name(), &module.name, &addr);
                match self.module_private_allocated_globals
                    .entry(module.name.clone())
                    .or_default()
                    .entry(global.get_name())
                {
                    Entry::Vacant(entry) => entry.insert(addr),
                    Entry::Occupied(_) => panic!("Duplicate definitions found for module-private global variable or function {:?} in module {:?}", global.get_name(), &module.name),
                };
                AllocationResult::ModulePrivate
            },
            Linkage::External => {
                // Public global, strong definition
                debug!("Allocating {:?} (public, strong) at {:?}", global.get_name(), &addr);
                match self.allocated_globals.entry(global.get_name()) {
                    Entry::Vacant(entry) => {
                        entry.insert(Definition::Strong(addr));
                    },
                    Entry::Occupied(mut entry) => {
                        match entry.get() {
                            Definition::Strong(_) => panic!("Duplicate strong definitions found for public global variable or function {:?}", global.get_name()),
                            Definition::Weak(_) => entry.insert(Definition::Strong(addr)),  // discard the weak definition in favor of this strong one
                        };
                    },
                };
                AllocationResult::Public
            },
            Linkage::AvailableExternally
            | Linkage::LinkOnceAny
            | Linkage::WeakAny
            | Linkage::Common
            | Linkage::ExternalWeak
            | Linkage::LinkOnceODR
            | Linkage::WeakODR => {
                // We treat all of these modes as "Public global, weak definition" under our semantics
                match self.allocated_globals.entry(global.get_name()) {
                    Entry::Vacant(entry) => {
                        debug!("Allocating {:?} (public, weak) at {:?}", global.get_name(), &addr);
                        entry.insert(Definition::Weak(addr));
                        AllocationResult::Public
                    },
                    Entry::Occupied(_) => {
                        // don't override an existing definition. If the existing definition
                        // was weak, we arbitrarily choose to leave it rather than replace it
                        // with this weak definition.
                        debug!("Skipping definition of {:?} (public, weak) as already defined", global.get_name());
                        AllocationResult::NoAllocate
                    },
                }
            },
            _ => unimplemented!("Linkage type {:?}", global.get_linkage()),
        }
    }

    /// Get the address at which the global variable or function with the given
    /// `Name` has been allocated; or `None` if not found.
    ///
    /// `module`: The `Module` in which the `Name` appeared. Note that modules
    /// may have their own module-private globals with the same name, so the name
    /// alone is not sufficient to identify a unique global.
    pub fn get_global_address(&self, name: &Name, module: &Module) -> Option<&B::BV> {
        // First look for a module-private definition. We allow this to have precedence over any public definition that may exist.
        self.module_private_allocated_globals
            .get(&module.name)
            .and_then(|hm| hm.get(name))
            .or_else(|| {
                // Module-private definition not found. Look for a public definition
                self.allocated_globals.get(name).map(Definition::get)
            })
    }

    /// Get the address at which the given `FunctionHook` has been allocated; or
    /// `None` if not found.
    pub fn get_function_hook_address(&self, hook: &FunctionHook<'p, B>) -> Option<&B::BV> {
        self.allocated_hooks.get(hook)
    }

    /// Given an address, get the `Callable` which was allocated at that address;
    /// or `None` if no `Callable` was allocated at that address.
    ///
    /// `module`: The `Module` in which the address appeared. Note that modules
    /// may have their own module-private functions with the same name, so the
    /// name alone is not sufficient to identify a unique global.
    pub fn get_func_for_address(&self, addr: u64, module: &Module) -> Option<Callable<'p, B>> {
        self.addr_to_function
            .get(&addr)
            .cloned()
            .or_else(|| {
                self.module_private_addr_to_function
                    .get(&module.name)
                    .and_then(|hm| hm.get(&addr).cloned())
            })
    }

    /// Adapt the `GlobalAllocations` to a new solver instance.
    ///
    /// The new solver instance should have been created (possibly transitively)
    /// via `SolverRef::duplicate()` from the `SolverRef` which the vars in the
    /// `GlobalAllocations` were originally created with (or most recently
    /// changed to). Further, no new variables should have been allocated since
    /// the call to `SolverRef::duplicate()`.
    pub fn change_solver(&mut self, new_solver: B::SolverRef) {
        for def in self.allocated_globals.values_mut() {
            let new_bv = new_solver.match_bv(def.get()).unwrap();
            *def = match def {
                Definition::Strong(_) => Definition::Strong(new_bv),
                Definition::Weak(_) => Definition::Weak(new_bv),
            };
        }
        for bv in self.allocated_hooks.values_mut() {
            *bv = new_solver.match_bv(bv).unwrap();
        }
        for hm in self.module_private_allocated_globals.values_mut() {
            for bv in hm.values_mut() {
                *bv = new_solver.match_bv(bv).unwrap();
            }
        }
    }
}
