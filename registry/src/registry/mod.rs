pub(crate) mod archive;
pub(crate) mod authz;
pub(crate) mod queries;
pub(crate) mod types;

pub(crate) fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("slug must not be empty".to_string());
    }
    let first = slug.chars().next().expect("checked non-empty");
    if !first.is_ascii_lowercase() {
        return Err("slug must start with a lowercase ASCII letter".to_string());
    }
    if slug.chars().count() > 64 {
        return Err("slug must be at most 64 characters".to_string());
    }
    if !slug
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err("slug may only contain lowercase letters, digits, and hyphens".to_string());
    }
    if slug.contains("--") {
        return Err("slug must not contain consecutive hyphens".to_string());
    }
    if slug.ends_with('-') {
        return Err("slug must not end with a hyphen".to_string());
    }
    Ok(())
}
