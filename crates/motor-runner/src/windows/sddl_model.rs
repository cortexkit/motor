//! Text-level edits of SDDL for in-memory "what if" descriptors. Used by the
//! read-only AppContainer access screen; nothing here touches a live object.
//! Parsing is conservative: anything it cannot isolate exactly makes `parse`
//! return None, so a model is never guessed.

/// ALL APPLICATION PACKAGES; its SDDL alias is "AC".
const ALL_APPLICATION_PACKAGES: &str = "S-1-15-2-1";

/// An SDDL string split into the part up to and including the DACL flags, the
/// DACL's ACE strings in order, and the SACL part (mandatory label) after them.
#[derive(Debug, PartialEq)]
pub(crate) struct Sddl {
    head: String,
    aces: Vec<String>,
    tail: String,
}
impl Sddl {
    /// Returns None when an ACE cannot be isolated safely (for example a
    /// conditional ACE with nested parentheses).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let start = text.find("D:")? + 2;
        let rest = &text[start..];
        let mut cursor = rest
            .find('(')
            .unwrap_or(rest.len())
            .min(rest.find("S:").unwrap_or(rest.len()));
        let head = text[..start + cursor].to_owned();
        let mut aces = Vec::new();
        while rest[cursor..].starts_with('(') {
            let end = cursor + rest[cursor..].find(')')?;
            let ace = &rest[cursor..=end];
            if ace[1..].contains('(') || ace[1..ace.len() - 1].split(';').count() < 6 {
                return None;
            }
            aces.push(ace.to_owned());
            cursor = end + 1;
        }
        let tail = rest[cursor..].to_owned();
        (tail.is_empty() || tail.starts_with("S:")).then_some(Self { head, aces, tail })
    }
    fn field(ace: &str, index: usize) -> &str {
        ace[1..ace.len() - 1].split(';').nth(index).unwrap_or("")
    }
    pub(crate) fn render(&self) -> String {
        format!("{}{}{}", self.head, self.aces.concat(), self.tail)
    }
    /// The descriptor as a Less Privileged AppContainer process sees it: ACEs
    /// for ALL APPLICATION PACKAGES are ignored, so they are removed.
    pub(crate) fn without_all_application_packages(&self) -> Self {
        Self {
            head: self.head.clone(),
            aces: self
                .aces
                .iter()
                .filter(|ace| {
                    let sid = Self::field(ace, 5);
                    sid != "AC" && sid != ALL_APPLICATION_PACKAGES
                })
                .cloned()
                .collect(),
            tail: self.tail.clone(),
        }
    }
    /// Adds allow ACEs after any explicit deny ACEs (so a deny still wins, as
    /// with New-RunnerPrepAcl; order among allows does not change decisions)
    /// and, when given, replaces the SACL with that mandatory label.
    pub(crate) fn with_grants(&self, grants: &[String], label: Option<&str>) -> Self {
        let index = self
            .aces
            .iter()
            .position(|ace| !(Self::field(ace, 0) == "D" && !Self::field(ace, 1).contains("ID")))
            .unwrap_or(self.aces.len());
        let mut aces = self.aces.clone();
        aces.splice(index..index, grants.iter().cloned());
        Self {
            head: self.head.clone(),
            aces,
            tail: label.map_or_else(|| self.tail.clone(), |label| format!("S:{label}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SAMPLE: &str = "O:SYG:SYD:PAI(D;;WD;;;S-1-5-7)(A;;0x20;;;S-1-5-21-1)(A;OICIID;FA;;;AC)(A;OICIID;FA;;;S-1-15-2-1)(A;OICIID;FA;;;SY)S:(ML;;NW;;;LW)";

    #[test]
    fn sddl_model_round_trips_unchanged_text() {
        assert_eq!(Sddl::parse(SAMPLE).unwrap().render(), SAMPLE);
        let empty = "O:SYG:SYD:S:(ML;;NW;;;LW)";
        assert_eq!(Sddl::parse(empty).unwrap().render(), empty);
        assert_eq!(Sddl::parse("O:SYG:SYD:P").unwrap().render(), "O:SYG:SYD:P");
    }
    #[test]
    fn sddl_model_lpac_view_drops_only_all_application_packages() {
        let lpac = Sddl::parse(SAMPLE)
            .unwrap()
            .without_all_application_packages()
            .render();
        assert_eq!(
            lpac,
            "O:SYG:SYD:PAI(D;;WD;;;S-1-5-7)(A;;0x20;;;S-1-5-21-1)(A;OICIID;FA;;;SY)S:(ML;;NW;;;LW)"
        );
        let restricted = "O:SYG:SYD:(A;;FA;;;S-1-15-2-2)";
        assert_eq!(
            Sddl::parse(restricted)
                .unwrap()
                .without_all_application_packages()
                .render(),
            restricted
        );
    }
    #[test]
    fn sddl_model_grants_follow_explicit_denies_and_label_replaces_sacl() {
        let grants = ["(A;OICI;0x1301bf;;;S-1-15-2-9)".to_owned()];
        let text = Sddl::parse(SAMPLE)
            .unwrap()
            .with_grants(&grants, Some("(ML;OICI;NW;;;LW)"))
            .render();
        assert_eq!(text, "O:SYG:SYD:PAI(D;;WD;;;S-1-5-7)(A;OICI;0x1301bf;;;S-1-15-2-9)(A;;0x20;;;S-1-5-21-1)(A;OICIID;FA;;;AC)(A;OICIID;FA;;;S-1-15-2-1)(A;OICIID;FA;;;SY)S:(ML;OICI;NW;;;LW)");
        let no_dacl_aces = Sddl::parse("O:SYG:SYD:S:(ML;;NW;;;LW)")
            .unwrap()
            .with_grants(&grants, None)
            .render();
        assert_eq!(
            no_dacl_aces,
            "O:SYG:SYD:(A;OICI;0x1301bf;;;S-1-15-2-9)S:(ML;;NW;;;LW)"
        );
    }
    #[test]
    fn sddl_model_refuses_what_it_cannot_isolate() {
        assert_eq!(
            Sddl::parse("O:SYG:SYD:(XA;;FA;;;WD;(Member_of {SID(BA)}))"),
            None
        );
        assert_eq!(Sddl::parse("O:SYG:SY"), None);
        assert_eq!(Sddl::parse("O:SYG:SYD:(A;;FA;;;SY"), None);
        assert_eq!(Sddl::parse("O:SYG:SYD:(A;;FA)"), None);
    }
}
