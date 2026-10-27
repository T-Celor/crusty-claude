//! Replacement of findings by vault placeholders.

use std::collections::HashSet;

use crusty_core::vault::SecretVault;

use crate::detect::Finding;

/// Distinct values masked by one call, split by kind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MaskCounts {
    /// Distinct secret values masked.
    pub secrets: HashSet<String>,
    /// Distinct personal-data values masked.
    pub pii: HashSet<String>,
}

impl MaskCounts {
    /// Adds the values of `other` to `self`.
    pub fn merge(&mut self, other: MaskCounts) {
        self.secrets.extend(other.secrets);
        self.pii.extend(other.pii);
    }
}

/// Replaces each finding in `text` with the placeholder of its value.
///
/// `findings` must come from [`crate::scan`] on the same `text`. Findings that overlap an
/// earlier one, or that do not fall on character boundaries, are skipped.
pub fn mask_text(text: &str, findings: &[Finding], vault: &SecretVault) -> (String, MaskCounts) {
    let mut out = String::with_capacity(text.len());
    let mut counts = MaskCounts::default();
    let mut last = 0;
    for f in findings {
        if f.start < last || f.end <= f.start {
            continue;
        }
        let Some(value) = text.get(f.start..f.end) else { continue };
        out.push_str(text.get(last..f.start).unwrap_or_default());
        out.push_str(&vault.placeholder(value, f.kind));
        let set = if f.kind.is_secret() { &mut counts.secrets } else { &mut counts.pii };
        set.insert(value.to_owned());
        last = f.end;
    }
    out.push_str(text.get(last..).unwrap_or_default());
    (out, counts)
}
