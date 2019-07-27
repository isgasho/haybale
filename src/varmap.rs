use crate::double_keyed_map::DoubleKeyedMap;
use std::fmt;
use z3::ast::{Ast, BV, Bool};
use log::debug;

use llvm_ir::Name;

#[derive(Clone)]
pub struct VarMap<'ctx> {
    ctx: &'ctx z3::Context,
    /// Maps a `Name` to the Z3 object corresponding to the active version of that `Name`.
    /// Different variables in different functions can have the same `Name` but different
    /// values, so we actually have (String, Name) as the key type where the String is the
    /// function name. We assume no two functions have the same name.
    active_version: DoubleKeyedMap<String, Name, BVorBool<'ctx>>,
    /// Maps a `Name` to the version number of the latest version of that `Name`.
    /// E.g., for `Name`s that have been created once, we have 0 here.
    /// Like with the `active_version` map, the key type here includes the function name.
    /// The version number here may not correspond to the active version in the
    /// presence of recursion: when we return from a recursive call, the caller's
    /// versions of the variables are active, even though the callee's versions
    /// are the most recently created.
    version_num: DoubleKeyedMap<String, Name, usize>,
    /// Maximum version number of any given `Name`.
    /// This bounds the maximum number of distinct versions of any given `Name`,
    /// and thus can be used to bound loops, really crudely.
    /// Variables with the same `Name` in different functions do not share
    /// counters for this purpose - they can each have versions up to the
    /// `max_version_num`.
    max_version_num: usize,
}

/// Our `VarMap` stores both `BV`s and `Bool`s
#[derive(Clone, PartialEq, Eq)]
enum BVorBool<'ctx> {
    BV(BV<'ctx>),
    Bool(Bool<'ctx>),
}

impl<'ctx> fmt::Debug for BVorBool<'ctx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            BVorBool::BV(bv) => write!(f, "BV( {} )", bv),
            BVorBool::Bool(b) => write!(f, "Bool( {} )", b),
        }
    }
}

impl<'ctx> From<BV<'ctx>> for BVorBool<'ctx> {
    fn from(bv: BV<'ctx>) -> BVorBool<'ctx> {
        BVorBool::BV(bv)
    }
}

impl<'ctx> From<Bool<'ctx>> for BVorBool<'ctx> {
    fn from(b: Bool<'ctx>) -> BVorBool<'ctx> {
        BVorBool::Bool(b)
    }
}

impl<'ctx> From<BVorBool<'ctx>> for BV<'ctx> {
    fn from(b: BVorBool<'ctx>) -> BV<'ctx> {
        match b {
            BVorBool::BV(bv) => bv,
            _ => panic!("Can't convert {:?} to BV", b),
        }
    }
}

impl<'ctx> From<BVorBool<'ctx>> for Bool<'ctx> {
    fn from(b: BVorBool<'ctx>) -> Bool<'ctx> {
        match b {
            BVorBool::Bool(b) => b,
            _ => panic!("Can't convert {:?} to Bool", b),
        }
    }
}

impl<'ctx> BVorBool<'ctx> {
    fn is_bv(&self) -> bool {
        match self {
            BVorBool::BV(_) => true,
            _ => false,
        }
    }

    fn is_bool(&self) -> bool {
        match self {
            BVorBool::Bool(_) => true,
            _ => false,
        }
    }
}

// these are basically From impls, but for converting ref to ref
impl<'ctx> BVorBool<'ctx> {
    fn as_bv(&self) -> &BV<'ctx> {
        match self {
            BVorBool::BV(bv) => &bv,
            _ => panic!("Can't convert {:?} to BV", self),
        }
    }

    fn as_bool(&self) -> &Bool<'ctx> {
        match self {
            BVorBool::Bool(b) => &b,
            _ => panic!("Can't convert {:?} to Bool", self),
        }
    }
}

// these are like the From impls, but make more of an effort to convert between
// types if the wrong type is requested
impl<'ctx> BVorBool<'ctx> {
    fn to_bv(self, ctx: &'ctx z3::Context) -> BV<'ctx> {
        match self {
            BVorBool::BV(bv) => bv,
            BVorBool::Bool(b) => b.ite(&BV::from_u64(ctx, 1, 1), &BV::from_u64(ctx, 0, 1)),
        }
    }

    fn to_bool(self, ctx: &'ctx z3::Context) -> Bool<'ctx> {
        match self {
            BVorBool::Bool(b) => b,
            BVorBool::BV(bv) => {
                if bv.get_size() == 1 {
                    bv._eq(&BV::from_u64(ctx, 1, 1))
                } else {
                    panic!("Can't convert BV {:?} of size {} to Bool", bv, bv.get_size())
                }
            },
        }
    }
}

impl<'ctx> VarMap<'ctx> {
    /// `max_versions_of_name`: Maximum number of distinct versions of any given `Name`.
    /// This can be used to bound loops (really crudely).
    /// Variables with the same `Name` in different functions do not share
    /// counters for this purpose - they can each have up to
    /// `max_versions_of_name` distinct versions.
    pub fn new(ctx: &'ctx z3::Context, max_versions_of_name: usize) -> Self {
        Self {
            ctx,
            active_version: DoubleKeyedMap::new(),
            version_num: DoubleKeyedMap::new(),
            max_version_num: max_versions_of_name - 1,  // because 0 is a version
        }
    }

    /// Create a new `BV` for the given `(String, Name)` pair.
    /// This function performs uniquing, so if you call it twice
    /// with the same `(String, Name)` pair, you will get two different `BV`s.
    /// Returns the new `BV`, or `Err` if it can't be created.
    /// (As of this writing, the only reason an `Err` might be returned is that
    /// creating the new `BV` would exceed `max_versions_of_name` -- see
    /// [`VarMap::new()`](struct.VarMap.html#method.new).)
    pub fn new_bv_with_name(&mut self, funcname: String, name: Name, bits: u32) -> Result<BV<'ctx>, &'static str> {
        let new_version = self.new_version_of_name(&funcname, &name)?;
        let bv = BV::new_const(self.ctx, new_version, bits);
        debug!("Adding bv var {:?} = {}", name, bv);
        self.active_version.insert(funcname, name, bv.clone().into());
        Ok(bv)
    }

    /// Create a new `Bool` for the given `(String, Name)` pair.
    /// This function performs uniquing, so if you call it twice
    /// with the same `(String, Name)` pair, you will get two different `Bool`s.
    /// Returns the new `Bool`, or `Err` if it can't be created.
    /// (As of this writing, the only reason an `Err` might be returned is that
    /// creating the new `Bool` would exceed `max_versions_of_name` -- see
    /// [`VarMap::new()`](struct.VarMap.html#method.new).)
    pub fn new_bool_with_name(&mut self, funcname: String, name: Name) -> Result<Bool<'ctx>, &'static str> {
        let new_version = self.new_version_of_name(&funcname, &name)?;
        let b = Bool::new_const(self.ctx, new_version);
        debug!("Adding bool var {:?} = {}", name, b);
        self.active_version.insert(funcname, name, b.clone().into());
        Ok(b)
    }

    /// Look up the most recent `BV` created for the given `(String, Name)` pair
    pub fn lookup_bv_var(&self, funcname: &String, name: &Name) -> BV<'ctx> {
        debug!("Looking up var {:?} from function {:?}", name, funcname);
        self.active_version.get(funcname, name).unwrap_or_else(|| {
            let keys: Vec<(&String, &Name)> = self.active_version.keys().collect();
            panic!("Failed to find var {:?} from function {:?} in map with keys {:?}", name, funcname, keys);
        }).clone().to_bv(self.ctx)
    }

    /// Look up the most recent `Bool` created for the given `(String, Name)` pair
    pub fn lookup_bool_var(&self, funcname: &String, name: &Name) -> Bool<'ctx> {
        debug!("Looking up var {:?} from function {:?}", name, funcname);
        self.active_version.get(funcname, name).unwrap_or_else(|| {
            let keys: Vec<(&String, &Name)> = self.active_version.keys().collect();
            panic!("Failed to find var {:?} from function {:?} in map with keys {:?}", name, funcname, keys);
        }).clone().to_bool(self.ctx)
    }

    /// Given a `Name` (from a particular function), creates a new version of it
    /// and returns the corresponding `z3::Symbol`
    /// (or `Err` if it would exceed the `max_version_num`)
    fn new_version_of_name(&mut self, funcname: &str, name: &Name) -> Result<z3::Symbol, &'static str> {
        let new_version_num = self.version_num.entry(funcname.to_owned(), name.clone())
            .and_modify(|v| *v += 1)  // increment if it already exists in map
            .or_insert(0);  // insert a 0 if it didn't exist in map
        if *new_version_num > self.max_version_num {
            return Err("Exceeded maximum number of versions of that `Name`");
        }
        //let mut suffix = "_".to_string();
        //suffix.push_str(&new_version_num.to_string());
        //let funcname_prefix: String = "@".to_owned() + funcname;
        let (name_prefix, stem): (&str, String) = match name {
            Name::Name(s) => ("name_", s.clone()),
            Name::Number(n) => ("%", n.to_string()),
        };
        //funcname_prefix.push_str(&stem);
        //funcname_prefix.push_str(&suffix);
        Ok(z3::Symbol::String("@".to_owned() + funcname + "_" + name_prefix + &stem + "_" + &new_version_num.to_string()))
    }

    /// Get a `RestoreInfo` which can later be used with `restore_fn_vars()` to
    /// restore all of the given function's variables (in their current active
    /// versions) back to active.
    ///
    /// This is intended to support recursion. A `RestoreInfo` can be generated
    /// before a recursive call, and then once the call returns, the restore
    /// operation ensures the caller still has access to the correct versions of
    /// its local variables (not the callee's versions, which are technically
    /// more recent).
    pub fn get_restore_info_for_fn(&self, funcname: String) -> RestoreInfo<'ctx> {
        let pairs_to_restore: Vec<_> = self.active_version.iter()
            .filter(|(f,_,_)| f == &&funcname)
            .map(|(_,n,v)| (n.clone(), v.clone()))
            .collect();
        RestoreInfo {
            funcname,
            pairs_to_restore,
        }
    }

    /// Restore all of the variables in a `RestoreInfo` to their versions which
    /// were active at the time the `RestoreInfo` was generated
    pub fn restore_fn_vars(&mut self, rinfo: RestoreInfo<'ctx>) {
        let funcname = rinfo.funcname.clone();
        for pair in rinfo.pairs_to_restore {
            let val = self.active_version
                .get_mut(&funcname, &pair.0)
                .unwrap_or_else(|| panic!("Malformed RestoreInfo: key {:?}", (&funcname, &pair.0)));
            *val = pair.1;
        }
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct RestoreInfo<'ctx> {
    funcname: String,
    pairs_to_restore: Vec<(Name, BVorBool<'ctx>)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_vars() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut varmap = VarMap::new(&ctx, 20);
        let funcname = "foo".to_owned();

        // create llvm-ir names
        let valname = Name::Name("val".to_owned());
        let boolname = Name::Number(2);

        // create corresponding Z3 values
        let valvar = varmap.new_bv_with_name(funcname.clone(), valname.clone(), 64).unwrap();
        let boolvar = varmap.new_bool_with_name(funcname.clone(), boolname.clone()).unwrap();  // these clone()s wouldn't normally be necessary but we want to compare against the original values later

        // check that looking up the llvm-ir values gives the correct Z3 ones
        assert_eq!(varmap.lookup_bv_var(&funcname, &valname), valvar);
        assert_eq!(varmap.lookup_bool_var(&funcname, &boolname), boolvar);
    }

    #[test]
    fn vars_are_uniqued() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut varmap = VarMap::new(&ctx, 20);
        let mut solver = crate::solver::Solver::new(&ctx);
        let funcname = "foo".to_owned();

        // create two vars with the same name
        let name = Name::Name("x".to_owned());
        let x1 = varmap.new_bv_with_name(funcname.clone(), name.clone(), 64).unwrap();
        let x2 = varmap.new_bv_with_name(funcname.clone(), name, 64).unwrap();

        // constrain with incompatible constraints
        solver.assert(&x1.bvugt(&BV::from_u64(&ctx, 2, 64)));
        solver.assert(&x2.bvult(&BV::from_u64(&ctx, 1, 64)));

        // check that we're still sat
        assert!(solver.check());

        // now repeat with integer names
        let name = Name::Number(3);
        let x1 = varmap.new_bv_with_name(funcname.clone(), name.clone(), 64).unwrap();
        let x2 = varmap.new_bv_with_name(funcname.clone(), name, 64).unwrap();
        solver.assert(&x1.bvugt(&BV::from_u64(&ctx, 2, 64)));
        solver.assert(&x2.bvult(&BV::from_u64(&ctx, 1, 64)));
        assert!(solver.check());

        // now repeat with the same name but different functions
        let name = Name::Number(10);
        let otherfuncname = "bar".to_owned();
        let x1 = varmap.new_bv_with_name(funcname.clone(), name.clone(), 64).unwrap();
        let x2 = varmap.new_bv_with_name(otherfuncname.clone(), name.clone(), 64).unwrap();
        solver.assert(&x1.bvugt(&BV::from_u64(&ctx, 2, 64)));
        solver.assert(&x2.bvult(&BV::from_u64(&ctx, 1, 64)));
        assert!(solver.check());
    }

    #[test]
    fn enforces_max_version() {
        let ctx = z3::Context::new(&z3::Config::new());

        // Create a `VarMap` with `max_version_num = 10`
        let mut varmap = VarMap::new(&ctx, 10);

        // Check that we can create 10 versions of the same `Name`
        let funcname = "foo".to_owned();
        let name = Name::Number(7);
        for _ in 0 .. 10 {
            let bv = varmap.new_bv_with_name(funcname.clone(), name.clone(), 64);
            assert!(bv.is_ok());
        }

        // Check that we can create another 10 versions of that `Name` in a different function
        let funcname2 = "bar".to_owned();
        for _ in 0 .. 10 {
            let bv = varmap.new_bv_with_name(funcname2.clone(), name.clone(), 64);
            assert!(bv.is_ok());
        }

        // Check that we can't create an 11th version of that `Name`
        let bv = varmap.new_bv_with_name(funcname, name, 64);
        assert!(bv.is_err());
    }
}
