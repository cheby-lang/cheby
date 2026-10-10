//! Symbols of REPL session items (D-456, D-547, plan §4.4, E4).
//!
//! Each REPL entry is a module of its own, `<repl:N>`, but its items belong
//! to the session: their symbols use the module path `<repl>` and carry the
//! item's generation, `$r<gen>` appended when it is above 0, as
//! `cb$fn$<repl>$f$r1` or `cb$layout$<repl>$Circle$r2`. The generation counts
//! the definitions of the name compiled before this one, so a redefinition
//! never collides with what it replaces (GAP(repl-redefinition-symbols),
//! D-547).
//!
//! An indirection slot is identified by the item's session path (`<repl>::f`)
//! and its ABI signature (D-456). A redefinition the session marks as
//! taking over an earlier definition ([`SessionName::reuse`], the same type
//! signature) repoints that slot, so existing callers reach the new code
//! (D-043); any other gets a new slot `cb$slot$<repl>$f$r<gen>`, and code
//! compiled earlier keeps the old one. A constant or session value that
//! takes over an earlier one shares its slot `cb$const$…`, so readers see
//! the new value (D-547).
//!
//! Static objects (dictionaries, descriptors) are named by MIR from module
//! paths: in their names, `<repl:N>::X` becomes `<repl>::X$r<gen>`, a
//! descriptor of a session type `X` gets `X$r<gen>`, and the module part
//! `<repl:N>` becomes `<repl>`.

use cheby_iface::DefId;
use cheby_mir::DefName;
use rustc_hash::FxHashMap;

/// The module path of every session item's symbols.
pub const SESSION_MODULE: &str = "<repl>";

/// How a REPL session names one item of an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionName {
    /// The entry's module path, `<repl:N>`, as MIR names the item.
    pub module: Box<str>,
    /// The item's name within the session: `f`, `T`, `I::f`.
    pub item: Box<str>,
    /// How many definitions of the name were compiled before this one.
    pub generation: u32,
    /// The earlier definition whose slot this one takes over: a function
    /// or constant with the same signature (D-043, D-547). For a function,
    /// the slot of the same path and ABI signature is repointed.
    pub reuse: Option<DefId>,
}

impl SessionName {
    /// The item's name with its generation: `f`, `f$r1`.
    #[must_use]
    pub fn versioned(&self) -> String {
        versioned(&self.item, self.generation)
    }

    /// The name symbols are built from: `<repl>` and the versioned item.
    #[must_use]
    pub fn def_name(&self, def: DefId) -> DefName {
        DefName {
            def,
            module: SESSION_MODULE.into(),
            item: self.versioned().into(),
        }
    }

    /// The path that identifies the item's slot across generations:
    /// `<repl>::f`.
    #[must_use]
    pub fn slot_path(&self) -> String {
        format!("{SESSION_MODULE}::{}", self.item)
    }
}

fn versioned(item: &str, generation: u32) -> String {
    if generation == 0 {
        item.to_owned()
    } else {
        format!("{item}$r{generation}")
    }
}

/// The session names registered with a [`crate::Decls`].
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionNames {
    by_def: FxHashMap<DefId, SessionName>,
    /// `(entry module path, item)` to generation, for static objects,
    /// which MIR names by paths.
    by_path: FxHashMap<(Box<str>, Box<str>), u32>,
}

impl SessionNames {
    pub(crate) fn get(&self, def: DefId) -> Option<&SessionName> {
        self.by_def.get(&def)
    }

    /// Registers `name` for `def`; returns the name it had before.
    pub(crate) fn insert(&mut self, def: DefId, name: SessionName) -> Option<SessionName> {
        self.by_path
            .insert((name.module.clone(), name.item.clone()), name.generation);
        self.by_def.insert(def, name)
    }

    fn generation(&self, module: &str, item: &str) -> Option<u32> {
        self.by_path.get(&(module.into(), item.into())).copied()
    }

    /// The module and name of a static object's symbol, rewritten when its
    /// module is a session entry; `None` when nothing changes.
    pub(crate) fn static_parts(
        &self,
        is_desc: bool,
        module: &str,
        name: &str,
    ) -> Option<(String, String)> {
        let in_session = is_entry_module(module);
        let mut renamed = self.rewrite_paths(name);
        if is_desc && in_session {
            // A nominal descriptor `T`, or `dyn$I` for `dyn I`, of an item
            // of `module`.
            let (prefix, item) = name
                .strip_prefix("dyn$")
                .map_or(("", name), |i| ("dyn$", i));
            match self.generation(module, item) {
                Some(generation) => {
                    renamed = Some(format!("{prefix}{}", versioned(item, generation)));
                }
                // A nominal descriptor of an item that is not a session
                // item: keep the entry's own module path.
                None if is_item_name(item) => return renamed.map(|n| (module.to_owned(), n)),
                // A structural descriptor, such as `(Int, String)`, moves to
                // `<repl>` like the dictionaries that point at it: its
                // name determines its contents (D-580), so every entry
                // shares one static and later entries import it (D-523).
                None => {}
            }
        }
        if !in_session && renamed.is_none() {
            return None;
        }
        let module = if in_session { SESSION_MODULE } else { module };
        Some((
            module.to_owned(),
            renamed.unwrap_or_else(|| name.to_owned()),
        ))
    }

    /// `text` with every `<repl:N>::X` of a registered item `X` written
    /// `<repl>::X$r<gen>`; `None` if there is none.
    fn rewrite_paths(&self, text: &str) -> Option<String> {
        const OPEN: &str = "<repl:";
        if !text.contains(OPEN) {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        let mut changed = false;
        while let Some(at) = rest.find(OPEN) {
            let (before, from) = rest.split_at(at);
            out.push_str(before);
            let Some(close) = from.find('>') else {
                out.push_str(from);
                rest = "";
                break;
            };
            let (module, after) = from.split_at(close.saturating_add(1));
            let Some(tail) = after.strip_prefix("::") else {
                out.push_str(module);
                rest = after;
                continue;
            };
            let len = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            let (item, remainder) = tail.split_at(len);
            if let Some(generation) = self.generation(module, item) {
                out.push_str(SESSION_MODULE);
                out.push_str("::");
                out.push_str(&versioned(item, generation));
                changed = true;
            } else {
                out.push_str(module);
                out.push_str("::");
                out.push_str(item);
            }
            rest = remainder;
        }
        out.push_str(rest);
        changed.then_some(out)
    }
}

/// Whether `name` is a bare item name, as a nominal descriptor's is, rather
/// than a structural type such as `(Int, String)` or `Option<Int>`.
fn is_item_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `module` is the path of a REPL entry module, `<repl:N>`.
fn is_entry_module(module: &str) -> bool {
    module.starts_with("<repl:") && module.ends_with('>')
}

#[cfg(test)]
mod tests {
    use cheby_iface::{DefIndex, ModuleId};

    use super::*;

    fn def(n: u32) -> DefId {
        DefId::new(ModuleId::from_u32(9), DefIndex::from_u32(n))
    }

    fn names() -> SessionNames {
        let mut names = SessionNames::default();
        for (n, module, item, generation) in [
            (0, "<repl:3>", "T", 1),
            (1, "<repl:3>", "I", 0),
            (2, "<repl:4>", "U", 2),
        ] {
            names.insert(
                def(n),
                SessionName {
                    module: module.into(),
                    item: item.into(),
                    generation,
                    reuse: None,
                },
            );
        }
        names
    }

    #[test]
    fn items_carry_their_generation() {
        let name = names().get(def(0)).cloned().unwrap();
        assert_eq!(name.versioned(), "T$r1");
        assert_eq!(name.slot_path(), "<repl>::T");
        let d = name.def_name(def(0));
        assert_eq!((&*d.module, &*d.item), ("<repl>", "T$r1"));
        assert_eq!(
            crate::mangle::item(crate::mangle::Kind::Fn, &d),
            "cb$fn$<repl>$T$r1"
        );
    }

    #[test]
    fn static_objects_of_entries_are_renamed() {
        let names = names();
        assert_eq!(
            names.static_parts(true, "<repl:3>", "T"),
            Some(("<repl>".to_owned(), "T$r1".to_owned()))
        );
        assert_eq!(
            names.static_parts(true, "<repl:3>", "dyn$I"),
            Some(("<repl>".to_owned(), "dyn$I".to_owned()))
        );
        assert_eq!(
            names.static_parts(false, "<repl:7>", "std::fmt::Show$<repl:4>::U"),
            Some((
                "<repl>".to_owned(),
                "std::fmt::Show$<repl>::U$r2".to_owned()
            ))
        );
        assert_eq!(
            names.static_parts(false, "app", "app::I$<repl:3>::T"),
            Some(("app".to_owned(), "app::I$<repl>::T$r1".to_owned()))
        );
        assert_eq!(names.static_parts(false, "app", "app::I$app::T"), None);
        // An unknown entry item keeps its entry's path.
        assert_eq!(names.static_parts(true, "<repl:5>", "V"), None);
        // Structural descriptors are shared by every entry.
        for module in ["<repl:5>", "<repl:6>"] {
            assert_eq!(
                names.static_parts(true, module, "(Int, String)"),
                Some(("<repl>".to_owned(), "(Int, String)".to_owned()))
            );
            assert_eq!(
                names.static_parts(true, module, "Option<<repl:4>::U>"),
                Some(("<repl>".to_owned(), "Option<<repl>::U$r2>".to_owned()))
            );
        }
    }
}
