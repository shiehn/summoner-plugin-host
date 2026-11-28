//! AU parameter descriptions, validity and ParameterList name refresh (safe).

use std::collections::HashSet;

use summoner_sys::au::{
    FLAG_NON_REAL_TIME, ParamInfo, UNIT_BOOLEAN, UNIT_CUSTOM, UNIT_DECIBELS, UNIT_HERTZ,
    UNIT_INDEXED, UNIT_MILLISECONDS, UNIT_PERCENT, UNIT_SECONDS,
};

use summoner_plugin::params::{LABEL_CHARS, cut_chars};
use summoner_proc::ParamMeta;

/// `valid[i]`: whether index i is the first index carrying its id.
pub(crate) fn first_occurrences(params: &[ParamInfo]) -> Vec<bool> {
    let mut seen: HashSet<u32> = HashSet::with_capacity(params.len());
    params.iter().map(|p| seen.insert(p.id)).collect()
}

/// One parameter's description. `order` is supplied by the caller (see "Tree order").
pub(crate) fn au_param_meta(info: &ParamInfo, order: u32) -> ParamMeta {
    let range = info.max - info.min;

    let default = if range > 0.0 && range.is_finite() {
        let d = ((info.default - info.min) / range).clamp(0.0, 1.0);
        if d.is_finite() { d } else { 0.0 }
    } else {
        0.0
    };

    let step_count = if (info.unit == UNIT_INDEXED || info.unit == UNIT_BOOLEAN)
        && range.is_finite()
        && range >= 0.0
    {
        range.round() as u32 // halves away from zero; `as` saturates
    } else {
        0
    };

    let label = match info.unit {
        u if u == UNIT_PERCENT => "%".to_owned(),
        u if u == UNIT_SECONDS => "s".to_owned(),
        u if u == UNIT_HERTZ => "Hz".to_owned(),
        u if u == UNIT_DECIBELS => "dB".to_owned(),
        u if u == UNIT_MILLISECONDS => "ms".to_owned(),
        u if u == UNIT_CUSTOM => info.unit_name.clone(),
        _ => String::new(),
    };

    ParamMeta::new(
        info.id,
        info.flags & FLAG_NON_REAL_TIME == 0,
        default,
        step_count,
        cut_chars(label, LABEL_CHARS),
        order,
    )
}

/// After a ParameterList notice: refresh, in place, the names the host answers with.
/// `loaded[i]` is index i's `(id, valid)`.
pub(crate) fn refresh_names(
    names: &mut [String],
    loaded: &[(u32, bool)],
    read: impl Fn(u32) -> Option<ParamInfo>,
) {
    for (name, &(id, valid)) in names.iter_mut().zip(loaded) {
        if !valid {
            continue;
        }
        if let Some(info) = read(id) {
            *name = info.name;
        }
    }
}
