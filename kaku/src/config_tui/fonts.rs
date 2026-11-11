//! Installed font families for the Font rows, so a font is picked from a
//! list instead of typed by name.

use std::collections::BTreeSet;

/// Fonts that ship inside the app bundle and so are always available, even
/// though CoreText does not list them.
pub(super) const BUNDLED: &[&str] = &["JetBrains Mono"];

/// Family names CoreText knows. With `monospace_only`, only families with a
/// fixed-pitch face, which is what a terminal grid wants. Hidden system
/// families (their names start with a dot) are left out.
pub(super) fn installed_families(monospace_only: bool) -> Vec<String> {
    let mut names = BTreeSet::new();
    let collection = core_text::font_collection::create_for_all_families();
    if let Some(descriptors) = collection.get_descriptors() {
        for descriptor in descriptors.iter() {
            if monospace_only {
                use core_text::font_descriptor::{SymbolicTraitAccessors, TraitAccessors};
                if !descriptor.traits().symbolic_traits().is_monospace() {
                    continue;
                }
            }
            let family = descriptor.family_name();
            if !family.is_empty() && !family.starts_with('.') {
                names.insert(family);
            }
        }
    }
    sort_families(names.into_iter().collect())
}

/// The bundled fonts first, then the rest in case-insensitive order.
pub(super) fn sort_families(mut families: Vec<String>) -> Vec<String> {
    families.retain(|f| !BUNDLED.contains(&f.as_str()));
    families.sort_by_key(|f| f.to_lowercase());
    families.dedup();
    let mut out: Vec<String> = BUNDLED.iter().map(|s| s.to_string()).collect();
    out.extend(families);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_lead_and_the_rest_sort_case_insensitively() {
        let sorted = sort_families(vec![
            "menlo".into(),
            "JetBrains Mono".into(),
            "Courier".into(),
            "Andale Mono".into(),
        ]);
        assert_eq!(
            sorted,
            vec!["JetBrains Mono", "Andale Mono", "Courier", "menlo"]
        );
    }

    #[test]
    fn the_system_lists_menlo_as_a_monospaced_family() {
        let mono = installed_families(true);
        assert!(mono.iter().any(|f| f == "Menlo"), "{:?}", mono);
        assert!(!mono.iter().any(|f| f == "Helvetica"));
        assert!(installed_families(false).iter().any(|f| f == "Helvetica"));
    }
}
