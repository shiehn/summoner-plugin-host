//! One VST3 factory class per name (the scan's factory path).

/// Records `name` and answers whether it is the first class of its name seen in this bundle's factory.
/// `seen` starts empty for each bundle.
///
/// The key is `name` with leading and trailing Unicode `White_Space` removed (`str::trim`). Two keys
/// are equal when they match after ASCII case folding only: every non-ASCII character compares by
/// exact code point, with no Unicode normalisation. The empty key is an ordinary key. Only the key is
/// recorded; the caller keeps the untrimmed name for the reported class.
pub(crate) fn first_class_of_name(seen: &mut Vec<String>, name: &str) -> bool {
    let key = name.trim();
    if seen.iter().any(|earlier| earlier.eq_ignore_ascii_case(key)) {
        return false;
    }
    seen.push(key.to_owned());
    true
}
