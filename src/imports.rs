use crate::{
  diagnostics::{Diagnostics, DiagnosticsConfig},
  fun::{load_book::do_parse_book, parser::ParseBook, Adt, Book, Name, Source, Term},
  imp::{Expr, Stmt},
};
use indexmap::map::Entry;
use indexmap::IndexMap;
use itertools::Itertools;
use std::{collections::HashSet, path::PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Imports {
  /// Imports declared in the program source.
  names: Vec<(Name, Vec<Name>)>,

  /// Map from binded names to source package.
  map: IndexMap<Name, Name>,

  /// Imported packages to be loaded in the program.
  /// When loaded, the book contents are drained to the parent book,
  /// adjusting def names accordingly.
  pkgs: Vec<(Name, ParseBook)>,
}

impl Imports {
  pub fn add_import(&mut self, import: Name, sub_imports: Vec<Name>) -> Result<(), Name> {
    if import.contains('@') && !import.contains('/') {
      return Err(import);
    }

    self.names.push((import, sub_imports));
    Ok(())
  }

  pub fn to_names(self) -> Vec<(Name, Vec<Name>)> {
    self.names
  }

  pub fn load_imports(&mut self, loader: &mut impl PackageLoader) -> Result<(), String> {
    for (src, sub_imports) in &self.names {
      let packages = loader.load_multiple(src.clone(), sub_imports)?;

      for (psrc, code) in packages {
        let mut module = do_parse_book(&code, &psrc, ParseBook::default())?;
        module.imports.load_imports(loader)?;
        self.pkgs.push((psrc, module));
      }

      if sub_imports.is_empty() {
        let (_namespace, name) = src.split_once('/').unwrap();

        if let Entry::Vacant(v) = self.map.entry(Name::new(name)) {
          v.insert(Name::new(format!("{}/{}", src, name)));
        }
      } else {
        for sub in sub_imports {
          if let Entry::Vacant(v) = self.map.entry(sub.clone()) {
            v.insert(Name::new(format!("{}/{}", src, sub)));
          }
        }
      }
    }

    Ok(())
  }
}

impl ParseBook {
  pub fn apply_imports(&mut self) -> Result<(), String> {
    self.apply_imports_go(None)
  }

  fn apply_imports_go(&mut self, main_imports: Option<&IndexMap<Name, Name>>) -> Result<(), String> {
    self.load_packages(main_imports)?;
    self.apply_import_binds(main_imports);
    Ok(())
  }

  /// Consumes the book imported packages,
  /// applying the imports recursively of every nested book.
  fn load_packages(&mut self, main_imports: Option<&IndexMap<Name, Name>>) -> Result<(), String> {
    for (src, mut package) in std::mem::take(&mut self.imports.pkgs) {
      // Can not be done outside the loop/function because of the borrow checker.
      // Just serves to pass only the import map of the first call to `apply_imports_go`.
      let main_imports = main_imports.unwrap_or(&self.imports.map);

      package.apply_imports_go(Some(main_imports))?;

      let new_adts = package.apply_adts(&src, main_imports);
      package.apply_defs(&src, main_imports);

      let book = package.to_fun()?;

      for (name, adt) in new_adts {
        self.add_adt(name, adt).expect("Package src should be unique, impossible to have name conflicts");
      }

      for def in book.defs.into_values() {
        if self.contains_def(&def.name) || self.ctrs.contains_key(&def.name) {
          unreachable!("Package src should be unique, impossible to have name conflicts");
        }

        self.fun_defs.insert(def.name.clone(), def);
      }
    }

    Ok(())
  }

  /// Applies a chain of `use bind = src` to every local definition.
  ///
  /// Must be used before `load_packages`
  fn apply_import_binds(&mut self, main_imports: Option<&IndexMap<Name, Name>>) {
    // Can not be done outside the function because of the borrow checker.
    // Just serves to pass only the import map of the first call to `apply_imports_go`.
    let main_imports = main_imports.unwrap_or(&self.imports.map);

    let mut local_imports: IndexMap<Name, Name> = IndexMap::new();

    // Collect local imports binds surrounded by `__` if not imported by the main book.
    for (bind, src) in self.imports.map.iter().rev() {
      let nam =
        if main_imports.values().contains(&src) { src.clone() } else { Name::new(format!("__{}__", src)) };

      if let Some(adt) = &self.adts.get(&nam) {
        for (ctr, _) in adt.ctrs.iter().rev() {
          let src = ctr.rsplit("__").nth(1).unwrap_or(ctr.as_ref());
          let nam = nam.rsplit("__").nth(1).unwrap_or(nam.as_ref());

          if let Some(a) = src.strip_prefix(nam) {
            let bind = Name::new(format!("{}{}", bind, a));
            local_imports.insert(bind, ctr.clone());
          }
        }
      } else {
        local_imports.insert(bind.clone(), nam);
      }
    }

    for def in self.fun_defs.values_mut().filter(|d| matches!(d.source, Source::Local(..))) {
      for rule in &mut def.rules {
        rule.body = std::mem::take(&mut rule.body).fold_uses(local_imports.iter());
      }
    }

    for (def, _) in self.imp_defs.values_mut().filter(|(_, source)| matches!(source, Source::Local(..))) {
      def.body = std::mem::take(&mut def.body).fold_uses(local_imports.iter());
    }
  }

  /// Consumes the book adts, applying the necessary naming transformations
  /// and adding `use ctr = ctr_src` chains to every local definition.
  ///
  /// Must be used before `apply_defs`
  fn apply_adts(&mut self, src: &Name, main_imp: &IndexMap<Name, Name>) -> IndexMap<Name, Adt> {
    let adts = std::mem::take(&mut self.adts);
    let mut new_adts = IndexMap::new();
    let mut ctrs_map = IndexMap::new();

    for (mut name, mut adt) in adts {
      match adt.source {
        Source::Local(..) => {
          adt.source = Source::Imported;
          name = Name::new(format!("{}/{}", src, name));

          let mangle_name = !main_imp.values().contains(&name);
          let mut mangle_adt_name = mangle_name;

          for (ctr, f) in std::mem::take(&mut adt.ctrs) {
            let mut ctr_name = Name::new(format!("{}/{}", src, ctr));

            let mangle_ctr = mangle_name && !main_imp.values().contains(&ctr_name);

            if mangle_ctr {
              mangle_adt_name = true;
              ctr_name = Name::new(format!("__{}__", ctr_name));
            }

            ctrs_map.insert(ctr, ctr_name.clone());
            adt.ctrs.insert(ctr_name, f);
          }

          if mangle_adt_name {
            name = Name::new(format!("__{}__", name));
          }
        }

        Source::Imported => {}

        Source::Builtin | Source::Generated => {
          unreachable!("No builtin or generated adt should be present at this step")
        }
      }

      new_adts.insert(name.clone(), adt);
    }

    for def in self.fun_defs.values_mut().filter(|d| matches!(d.source, Source::Local(..))) {
      for rule in &mut def.rules {
        rule.body = std::mem::take(&mut rule.body).fold_uses(ctrs_map.iter().rev());
      }
    }

    for (def, _) in self.imp_defs.values_mut().filter(|(_, source)| matches!(source, Source::Local(..))) {
      def.body = std::mem::take(&mut def.body).fold_uses(ctrs_map.iter().rev());
    }

    new_adts
  }

  /// Apply the necessary naming transformations to the book definitions,
  /// and adding `use def = def_src` chains to every local definition.
  fn apply_defs(&mut self, src: &Name, main_imp: &IndexMap<Name, Name>) {
    let mut def_map: IndexMap<_, _> = IndexMap::new();

    // Rename the definitions to their source name
    // Surrounded with `__` if not imported by the main book.
    for def in self.fun_defs.values_mut() {
      update_name(&mut def.name, def.source, src, main_imp, &mut def_map);
    }

    for (def, source) in self.imp_defs.values_mut() {
      update_name(&mut def.name, *source, src, main_imp, &mut def_map);
    }

    for (nam, def) in &mut self.fun_defs {
      if let Source::Local(..) = def.source {
        for rule in &mut def.rules {
          let bod = std::mem::take(&mut rule.body);
          rule.body = bod.fold_uses(def_map.iter().rev().filter(|(n, _)| n != &nam));
        }
        def.source = Source::Imported;
      }
    }

    for (nam, (def, source)) in &mut self.imp_defs {
      if let Source::Local(..) = source {
        let bod = std::mem::take(&mut def.body);
        def.body = bod.fold_uses(def_map.iter().rev().filter(|(n, _)| n != &nam));
        *source = Source::Imported;
      }
    }
  }
}

fn update_name(
  def_name: &mut Name,
  def_source: Source,
  src: &Name,
  main_imp: &IndexMap<Name, Name>,
  def_map: &mut IndexMap<Name, Name>,
) {
  match def_source {
    Source::Local(..) => {
      let mut new_name = Name::new(format!("{}/{}", src, def_name));

      if !main_imp.values().contains(&new_name) {
        new_name = Name::new(format!("__{}__", new_name));
      }

      def_map.insert(def_name.clone(), new_name.clone());
      *def_name = new_name;
    }

    Source::Imported => {}

    Source::Builtin | Source::Generated => {
      unreachable!("No builtin or generated definition should be present at this step")
    }
  }
}

impl Term {
  fn fold_uses<'a>(self, map: impl Iterator<Item = (&'a Name, &'a Name)>) -> Self {
    map.fold(self, |acc, (bind, nam)| Term::Use {
      nam: Some(bind.clone()),
      val: Box::new(Term::Var { nam: nam.clone() }),
      nxt: Box::new(acc),
    })
  }
}

impl Stmt {
  fn fold_uses<'a>(self, map: impl Iterator<Item = (&'a Name, &'a Name)>) -> Stmt {
    map.fold(self, |acc, (bind, nam)| Stmt::Use {
      nam: bind.clone(),
      val: Box::new(Expr::Var { nam: nam.clone() }),
      nxt: Box::new(acc),
    })
  }
}

pub trait PackageLoader {
  /// Loads a package.
  /// Should only return `Ok(None)` if the package is already loaded
  fn load(&mut self, name: Name) -> Result<Option<(Name, String)>, String>;
  fn load_multiple(&mut self, name: Name, sub_names: &[Name]) -> Result<Vec<(Name, String)>, String>;
  fn is_loaded(&self, name: &Name) -> bool;
}

pub struct DefaultLoader<T: Fn(&str) -> Result<String, String>> {
  pub local_path: Option<PathBuf>,
  pub loaded: HashSet<Name>,
  pub load_fn: T,
}

impl<T: Fn(&str) -> Result<String, String>> PackageLoader for DefaultLoader<T> {
  fn load(&mut self, name: Name) -> Result<Option<(Name, String)>, String> {
    if !self.is_loaded(&name) {
      self.loaded.insert(name.clone());
      (self.load_fn)(&name).map(|pack| Some((name, pack)))
    } else {
      Ok(None)
    }
  }

  fn load_multiple(&mut self, name: Name, sub_names: &[Name]) -> Result<Vec<(Name, String)>, String> {
    if name.contains('@') {
      let mut packages = Vec::new();

      if sub_names.is_empty() {
        if let Some(package) = self.load(name)? {
          packages.push(package)
        }
      } else {
        for sub in sub_names {
          if let Some(p) = self.load(Name::new(&(format!("{}/{}", name, sub))))? {
            packages.push(p);
          }
        }
      }

      Ok(packages)
    } else if let Some(path) = &self.local_path {
      // Loading local packages is different than non-local ones,
      // sub_names refer to top level definitions on the imported file.
      // This should match the behaviour of importing a uploaded version of the imported file,
      // as each def will be saved separately.

      if !self.is_loaded(&name) {
        // TODO: Should the local filesystem be searched anyway for each sub_name?
        self.loaded.insert(name.clone());
        let path = path.parent().unwrap().join(name.as_ref()).with_extension("bend");
        std::fs::read_to_string(path).map_err(|e| e.to_string()).map(|c| vec![(name, c)])
      } else {
        Ok(Vec::new())
      }
    } else {
      Err(format!(
        "Can not import local '{}'. Use 'version@{}' if you wish to import a online package.",
        name, name
      ))
    }
  }

  fn is_loaded(&self, name: &Name) -> bool {
    self.loaded.contains(name)
  }
}

#[allow(clippy::field_reassign_with_default)]
/// Check book without warnings about unused definitions
pub fn check_book(book: &mut Book) -> Result<Diagnostics, Diagnostics> {
  let mut diagnostics_cfg = DiagnosticsConfig::default();
  diagnostics_cfg.unused_definition = crate::diagnostics::Severity::Allow;
  let compile_opts = crate::CompileOpts::default();

  crate::check_book(book, diagnostics_cfg, compile_opts)
}
