//! Stable identity of one Windows startup entry.
//!
//! An entry is named by WHERE it lives, never by what it is called on screen:
//! source kind (Run value / Startup-folder file), scope (this user / all
//! users), registry view (64-bit Run key / 32-bit WOW6432Node Run key) and the
//! key (value name, or shortcut file name). Registry value names and NTFS file
//! names are case-insensitive, so comparison and hashing use the lower-cased
//! key while the original spelling is kept for writes.

use std::hash::{Hash, Hasher};

/// Longest key kept (registry value names can be longer; nothing sane is).
pub const MAX_KEY: usize = 260;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// a value under a `Run` key
    Run,
    /// a file in a Startup folder
    Folder,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Scope {
    /// HKCU / the user's own Startup folder
    User,
    /// HKLM / the common Startup folder (changing it needs administrator rights)
    AllUsers,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum View {
    /// the 64-bit `Run` key (or "not applicable" for a folder)
    Native,
    /// `WOW6432Node\...\Run` (all-users, 32-bit programs)
    Wow32,
}

#[derive(Clone, Debug)]
pub struct EntryId {
    pub kind: Kind,
    pub scope: Scope,
    pub view: View,
    /// value name or file name, original spelling
    pub key: String,
}

impl PartialEq for EntryId {
    fn eq(&self, o: &Self) -> bool {
        self.kind == o.kind && self.scope == o.scope && self.view == o.view && self.canon() == o.canon()
    }
}
impl Eq for EntryId {}
impl Hash for EntryId {
    fn hash<H: Hasher>(&self, h: &mut H) {
        (self.kind, self.scope, self.view).hash(h);
        self.canon().hash(h);
    }
}

impl EntryId {
    pub fn new(kind: Kind, scope: Scope, view: View, key: impl Into<String>) -> EntryId {
        EntryId { kind, scope, view, key: key.into() }
    }

    fn canon(&self) -> String {
        self.key.to_lowercase()
    }

    /// Changing this entry needs administrator rights (HKLM / common folder).
    pub fn machine(&self) -> bool {
        self.scope == Scope::AllUsers
    }

    /// Short human label of where it lives.
    pub fn source_label(&self) -> &'static str {
        match (self.kind, self.scope, self.view) {
            (Kind::Run, Scope::User, _) => "Run (this user)",
            (Kind::Run, Scope::AllUsers, View::Native) => "Run (all users)",
            (Kind::Run, Scope::AllUsers, View::Wow32) => "Run (all users, 32-bit)",
            (Kind::Folder, Scope::User, _) => "Startup folder (this user)",
            (Kind::Folder, Scope::AllUsers, _) => "Startup folder (all users)",
        }
    }

    /// Short scope word for a table column.
    pub fn scope_label(&self) -> &'static str {
        match self.scope {
            Scope::User => "This user",
            Scope::AllUsers => "All users",
        }
    }

    /// `kind|scope|view|key`, with `|`, `%` and control characters escaped so the
    /// text round-trips through a one-line config value.
    pub fn encode(&self) -> String {
        let k = match self.kind {
            Kind::Run => "run",
            Kind::Folder => "folder",
        };
        let s = match self.scope {
            Scope::User => "user",
            Scope::AllUsers => "all",
        };
        let v = match self.view {
            View::Native => "64",
            View::Wow32 => "32",
        };
        let mut key = String::with_capacity(self.key.len());
        for c in self.key.chars() {
            if c == '|' || c == '%' || c.is_control() {
                let mut b = [0u8; 4];
                for byte in c.encode_utf8(&mut b).bytes() {
                    key.push_str(&format!("%{byte:02X}"));
                }
            } else {
                key.push(c);
            }
        }
        format!("{k}|{s}|{v}|{key}")
    }

    pub fn parse(text: &str) -> Option<EntryId> {
        let mut it = text.splitn(4, '|');
        let kind = match it.next()? {
            "run" => Kind::Run,
            "folder" => Kind::Folder,
            _ => return None,
        };
        let scope = match it.next()? {
            "user" => Scope::User,
            "all" => Scope::AllUsers,
            _ => return None,
        };
        let view = match it.next()? {
            "64" => View::Native,
            "32" => View::Wow32,
            _ => return None,
        };
        let raw = it.next()?;
        // only a Run entry has a view; only an all-users Run entry has the 32-bit one
        if view == View::Wow32 && (kind != Kind::Run || scope != Scope::AllUsers) {
            return None;
        }
        let mut bytes = Vec::with_capacity(raw.len());
        let rb = raw.as_bytes();
        let mut i = 0;
        while i < rb.len() {
            if rb[i] == b'%' {
                let hex = raw.get(i + 1..i + 3)?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            } else {
                bytes.push(rb[i]);
                i += 1;
            }
        }
        let key = String::from_utf8(bytes).ok()?;
        if key.is_empty() || key.chars().count() > MAX_KEY || key.chars().any(|c| c.is_control()) {
            return None;
        }
        Some(EntryId { kind, scope, view, key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_awkward_keys() {
        for key in ["Discord", "a|b", "100%", "ünï ✓ .lnk", "x%7Cy"] {
            let id = EntryId::new(Kind::Run, Scope::AllUsers, View::Wow32, key);
            let back = EntryId::parse(&id.encode()).expect(key);
            assert_eq!(back.key, key);
            assert_eq!(back, id);
            assert!(!id.encode().contains('\n'));
        }
    }

    #[test]
    fn control_characters_never_become_an_identity() {
        // encode escapes them (one-line safe), parse refuses to read them back
        let id = EntryId::new(Kind::Run, Scope::User, View::Native, "tab\there");
        assert!(!id.encode().contains('\t'));
        assert!(EntryId::parse(&id.encode()).is_none());
    }

    #[test]
    fn identity_is_case_insensitive_but_keeps_spelling() {
        let a = EntryId::new(Kind::Run, Scope::User, View::Native, "OneDrive");
        let b = EntryId::new(Kind::Run, Scope::User, View::Native, "onedrive");
        assert_eq!(a, b);
        let mut h1 = std::collections::hash_map::DefaultHasher::new();
        let mut h2 = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut h1);
        b.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
        assert_eq!(b.key, "onedrive");
    }

    #[test]
    fn same_name_in_another_source_is_another_entry() {
        let a = EntryId::new(Kind::Run, Scope::User, View::Native, "Foo");
        assert_ne!(a, EntryId::new(Kind::Run, Scope::AllUsers, View::Native, "Foo"));
        assert_ne!(a, EntryId::new(Kind::Run, Scope::AllUsers, View::Wow32, "Foo"));
        assert_ne!(a, EntryId::new(Kind::Folder, Scope::User, View::Native, "Foo"));
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", "run|user|64|", "run|user|99|x", "bogus|user|64|x", "run|root|64|x", "run|user|64|%ZZ", "folder|user|32|a.lnk", "run|user|32|x", "run|user|64|a%0Ab"] {
            assert!(EntryId::parse(bad).is_none(), "{bad}");
        }
        assert!(EntryId::parse(&format!("run|user|64|{}", "x".repeat(MAX_KEY + 1))).is_none());
    }
}
