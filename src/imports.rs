use crate::{
  diagnostics::{Diagnostics, DiagnosticsConfig},
  fun::{load_book::do_parse_book, Book, Name, Source, Term},
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
  /// adjusting def names and refs accordingly.
  pkgs: Vec<(Name, Book)>,
}

impl Imports {
  pub fn add_import(&mut self, import: Name, sub_imports: Vec<Name>) -> Result<(), Name> {
    if import.contains('@') && !import.contains('/') {
      return Err(import);
    }

    self.names.push((import, sub_imports));
    Ok(())
  }

  pub fn imports(&self) -> &[(Name, Vec<Name>)] {
    &self.names
  }

  pub fn load_imports(&mut self, loader: &mut impl PackageLoader) -> Result<(), String> {
    for (src, sub_imports) in &self.names {
      let packages = loader.load_multiple(src.clone(), sub_imports)?;

      for (psrc, code) in packages {
        let mut module = do_parse_book(&code, &psrc, Book::default())?;
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

impl Book {
  pub fn apply_imports(&mut self) -> Result<(), Diagnostics> {
    self.apply_imports_go(None)
  }

  fn apply_imports_go(&mut self, main_imports: Option<&IndexMap<Name, Name>>) -> Result<(), Diagnostics> {
    // Can not be done on the outer function because of the borrow checker.
    // Just serves to pass only the import map of the first call to this function.
    let main_imports = main_imports.unwrap_or(&self.imports.map);

    // Local imports binds surrounded by `__` if not imported by the main book.
    let local_imports: IndexMap<Name, Name> = self
      .imports
      .map
      .iter()
      .rev()
      .map(|(bind, src)| {
        let nam =
          if main_imports.values().contains(&src) { src.clone() } else { Name::new(format!("__{}__", src)) };
        (bind.clone(), nam)
      })
      .collect();

    // Applies a chain of `use bind = src; nxt` to every normal definition
    // To be able to access the imported definitions.
    for def in self.defs.values_mut().filter(|d| matches!(d.source, Source::Normal(..))) {
      for rule in &mut def.rules {
        rule.body = fold_uses(std::mem::take(&mut rule.body), local_imports.iter());
      }
    }

    // TODO: Check for missing imports from local files
    // TODO: handle adts and ctrs
    for (src, package) in &mut self.imports.pkgs {
      package.apply_imports_go(Some(main_imports))?;

      let mut defs = std::mem::take(&mut package.defs);
      let mut map: IndexMap<_, _> = IndexMap::new();

      // Rename the definitions to their source name
      // Surrounded with `__` if not imported by the main book.
      for def in defs.values_mut() {
        if let Source::Normal(..) = def.source {
          let mut new_name = Name::new(format!("{}/{}", src, def.name));

          if !main_imports.values().contains(&new_name) {
            new_name = Name::new(format!("__{}__", new_name));
          }

          map.insert(def.name.clone(), new_name.clone());
          def.name = new_name;
        }
      }

      for (nam, mut def) in defs {
        // Applies a chain of `use bind = src; nxt` to every normal definition
        // To be able to access the definitions of its own book.
        if let Source::Normal(..) = def.source {
          for rule in &mut def.rules {
            rule.body =
              fold_uses(std::mem::take(&mut rule.body), map.iter().rev().filter(|(n, _)| *n != &nam));
          }
          def.source = Source::Imported;
        }

        match self.defs.entry(def.name.clone()) {
          Entry::Occupied(_) => todo!(), // TODO: Conflict of names between imported and local def
          Entry::Vacant(e) => _ = e.insert(def),
        }
      }
    }

    Ok(())
  }
}

fn fold_uses<'a>(bod: Term, map: impl Iterator<Item = (&'a Name, &'a Name)>) -> Term {
  map.fold(bod, |acc, (bind, nam)| Term::Use {
    nam: Some(bind.clone()),
    val: Box::new(Term::Var { nam: nam.clone() }),
    nxt: Box::new(acc),
  })
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
