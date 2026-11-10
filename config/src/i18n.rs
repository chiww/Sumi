//! Locale resolution for Kaku.
//!
//! The actual i18n bundle (rust-i18n) is registered per-binary, but the
//! *decision* of which locale to use is centralized here so every entry
//! point (kaku, kaku-gui, k) agrees on the same answer.

use std::env;

/// Locale string used when no configuration nor system environment yields
/// a usable hint. Matches `CFBundleDevelopmentRegion` in `Info.plist`.
pub const FALLBACK_LOCALE: &str = "en";

/// Sentinel value of `Config::language` that means "auto-detect from the
/// process environment". Anything else is honored verbatim.
pub const LANGUAGE_AUTO: &str = "auto";

/// Locales that Kaku currently ships translations for. Anything that
/// normalizes to one of these tags is accepted; everything else falls
/// back to [`FALLBACK_LOCALE`] with a `log::warn`.
const SUPPORTED_LOCALES: &[&str] = &["en", "zh-CN"];

/// Resolve the locale that Kaku should run under.
///
/// Resolution order:
/// 1. `config_language` (the value of `config.language` from `sumi.lua`),
///    unless it equals [`LANGUAGE_AUTO`] or is empty.
/// 2. `$LC_ALL`, `$LC_MESSAGES`, `$LANG` (first non-empty wins).
/// 3. [`FALLBACK_LOCALE`].
///
/// The return value is always one of [`SUPPORTED_LOCALES`]. Inputs that
/// resemble a supported locale but use a different separator (`zh_CN`,
/// `zh_CN.UTF-8`) are normalized.
pub fn resolve_locale(config_language: &str) -> String {
    resolve_locale_with(config_language, |var| env::var(var).ok())
}

/// [`resolve_locale`] with the environment lookup injected, so tests do not
/// race each other over the process-wide environment.
fn resolve_locale_with(config_language: &str, env_var: impl Fn(&str) -> Option<String>) -> String {
    if let Some(locale) = normalize(config_language.trim()) {
        return locale;
    }

    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = env_var(var) {
            if let Some(locale) = normalize(value.trim()) {
                log::trace!("i18n: resolved locale from {} = {:?}", var, locale);
                return locale;
            }
        }
    }

    FALLBACK_LOCALE.to_string()
}

/// Map free-form locale strings (`"zh_CN.UTF-8"`, `"zh-Hans"`,
/// `"zh-CN@whatever"`) to one of [`SUPPORTED_LOCALES`].
///
/// Returns `None` for the `auto` sentinel, empty input, and unsupported
/// locales. Unsupported locales additionally emit a `log::warn` so users
/// notice that their explicit override was ignored.
fn normalize(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.eq_ignore_ascii_case(LANGUAGE_AUTO) {
        return None;
    }

    // Strip codeset (`zh_CN.UTF-8` -> `zh_CN`) and modifier (`@latin`).
    let head = raw.split(['.', '@']).next().unwrap_or(raw);

    // Normalize separators: `zh_CN` -> `zh-CN`.
    let dashed = head.replace('_', "-");

    // Exact match against a supported tag wins.
    if let Some(hit) = SUPPORTED_LOCALES
        .iter()
        .find(|tag| tag.eq_ignore_ascii_case(&dashed))
    {
        return Some((*hit).to_string());
    }

    // Chinese script-family aliases that the system might hand us.
    let lower = dashed.to_ascii_lowercase();
    if matches!(lower.as_str(), "zh" | "zh-hans" | "zh-hans-cn" | "zh-cn") {
        return Some("zh-CN".to_string());
    }
    if lower.starts_with("en") {
        return Some("en".to_string());
    }

    log::warn!("i18n: locale {raw:?} is not supported, falling back to {FALLBACK_LOCALE:?}");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(config_language: &str, vars: &[(&str, &str)]) -> String {
        resolve_locale_with(config_language, |name| {
            vars.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn explicit_zh_cn_overrides_environment() {
        assert_eq!(resolve("zh-CN", &[("LANG", "en_US.UTF-8")]), "zh-CN");
    }

    #[test]
    fn auto_falls_through_to_environment() {
        assert_eq!(resolve("auto", &[("LANG", "zh_CN.UTF-8")]), "zh-CN");
    }

    #[test]
    fn empty_config_value_falls_through_to_environment() {
        assert_eq!(resolve("", &[("LANG", "en_US.UTF-8")]), "en");
    }

    #[test]
    fn missing_environment_falls_back_to_english() {
        assert_eq!(resolve("auto", &[]), FALLBACK_LOCALE);
    }

    #[test]
    fn unsupported_locale_falls_back_with_warning() {
        assert_eq!(resolve("xx-YY", &[]), FALLBACK_LOCALE);
    }

    #[test]
    fn lc_all_outranks_lang() {
        let vars = [("LANG", "en_US.UTF-8"), ("LC_ALL", "zh_CN.UTF-8")];
        assert_eq!(resolve("auto", &vars), "zh-CN");
    }

    #[test]
    fn chinese_script_aliases_normalize_to_zh_cn() {
        assert_eq!(resolve("zh-Hans", &[]), "zh-CN");
        assert_eq!(resolve("zh_Hans_CN", &[]), "zh-CN");
        assert_eq!(resolve("zh", &[]), "zh-CN");
    }
}
