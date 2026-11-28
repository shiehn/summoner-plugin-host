//! AU v2 parameter list and per-id parameter info (message thread only).

use std::ffi::{c_char, c_void};
use std::mem::{MaybeUninit, size_of};

use super::Instance;
use super::{
    CF_UTF8, FLAG_CF_NAME_RELEASE, FLAG_HAS_CF_NAME, FLAG_HAS_CLUMP, PROP_PARAMETER_INFO,
    PROP_PARAMETER_LIST, SCOPE_GLOBAL, UNIT_CUSTOM,
};

// ---- Raw declarations (the integrator may swap these for the crate's own bindings). ----
// Integration: the aliases that typed only the replaced extern blocks are removed.

type AudioUnitParameterID = u32;
type CFStringRef = *const c_void;

/// Bytes per AudioUnitParameterID (a type size, not an Apple constant).
const ID_BYTES: u32 = size_of::<AudioUnitParameterID>() as u32;

#[repr(C)]
struct AudioUnitParameterInfo {
    name: [c_char; 52],
    unit_name: CFStringRef,
    clump_id: u32,
    cf_name_string: CFStringRef,
    unit: u32,
    min_value: f32,
    max_value: f32,
    default_value: f32,
    flags: u32,
}

// Integration: the writer's two extern blocks are replaced by the crate's own declarations of the same functions
// (super::, identical signatures), as the writer allowed.
use super::{
    AudioUnitGetProperty, AudioUnitGetPropertyInfo, CFGetTypeID, CFRelease, CFStringGetCString,
    CFStringGetLength, CFStringGetMaximumSizeForEncoding, CFStringGetTypeID,
};

// ---- Public type ----

/// One parameter's `AudioUnitParameterInfo`, read into owned values.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    /// The `AudioUnitParameterID`.
    pub id: u32,
    /// The name (`cfNameString` with `kAudioUnitParameterFlag_HasCFNameString`, else the fixed `name` array).
    pub name: String,
    /// `minValue`.
    pub min: f32,
    /// `maxValue`.
    pub max: f32,
    /// `defaultValue`.
    pub default: f32,
    /// The `kAudioUnitParameterFlag_*` bits.
    pub flags: u32,
    /// The `AudioUnitParameterUnit` (`UNIT_*`).
    pub unit: u32,
    /// `clumpID`, when `kAudioUnitParameterFlag_HasClump` is set.
    pub clump: Option<u32>,
    /// A custom unit's `unitName` (`UNIT_CUSTOM`), else empty.
    pub unit_name: String,
}

impl Instance {
    /// The unit's parameter list, in its order, with each id's info. Ids whose info cannot be read are left out.
    /// Repeated ids are kept, at every position.
    pub fn params(&self) -> Vec<ParamInfo> {
        self.param_ids()
            .into_iter()
            .filter_map(|id| self.param_info(id))
            .collect()
    }

    /// One id's info, or `None` when it cannot be read.
    pub fn param_info(&self, id: u32) -> Option<ParamInfo> {
        let unit = self.unit();
        let mut raw = MaybeUninit::<AudioUnitParameterInfo>::zeroed();
        let mut size = size_of::<AudioUnitParameterInfo>() as u32;
        // SAFETY: `unit` is a created, initialised AudioUnit owned by `self`; `raw` is a writable,
        // zero-initialised AudioUnitParameterInfo of exactly `size` bytes, and `size` is a valid u32 out-pointer.
        let status = unsafe {
            AudioUnitGetProperty(
                unit,
                PROP_PARAMETER_INFO,
                SCOPE_GLOBAL,
                id,
                raw.as_mut_ptr().cast::<c_void>(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        // SAFETY: the struct was zero-initialised (all-zero is valid for every field: integers, floats,
        // null pointers), and the unit may only have written values of the declared field types.
        let raw = unsafe { raw.assume_init() };

        let flags = raw.flags;
        let is_custom = raw.unit == UNIT_CUSTOM;

        let name = if flags & FLAG_HAS_CF_NAME != 0 {
            cf_text(raw.cf_name_string)
        } else {
            fixed_name(&raw.name)
        };
        // Read before any release.
        let unit_name = if is_custom {
            cf_text(raw.unit_name)
        } else {
            String::new()
        };

        if flags & FLAG_CF_NAME_RELEASE != 0 {
            if !raw.cf_name_string.is_null() {
                // SAFETY: with CFNameRelease set the caller owns cfNameString (Apple's rule); it is non-null
                // and released exactly once here, after its text was copied out above.
                unsafe { CFRelease(raw.cf_name_string) };
            }
            if is_custom && !raw.unit_name.is_null() {
                // SAFETY: with CFNameRelease set and the custom unit, the caller owns unitName; it is
                // non-null and released exactly once here, after its text was copied out above.
                unsafe { CFRelease(raw.unit_name) };
            }
        }

        let clump = if flags & FLAG_HAS_CLUMP != 0 {
            Some(raw.clump_id)
        } else {
            None
        };

        Some(ParamInfo {
            id,
            name,
            min: raw.min_value,
            max: raw.max_value,
            default: raw.default_value,
            flags,
            unit: raw.unit,
            clump,
            unit_name,
        })
    }

    /// The raw ParameterList ids, in the unit's order (empty on any failure).
    fn param_ids(&self) -> Vec<AudioUnitParameterID> {
        let unit = self.unit();
        let mut size: u32 = 0;
        // SAFETY: `unit` is a valid initialised AudioUnit; `size` is a valid out-pointer; the writable
        // out-pointer may be null per the API.
        let status = unsafe {
            AudioUnitGetPropertyInfo(
                unit,
                PROP_PARAMETER_LIST,
                SCOPE_GLOBAL,
                0,
                &mut size,
                std::ptr::null_mut(),
            )
        };
        if status != 0 || size == 0 {
            return Vec::new();
        }
        let count = size / ID_BYTES;
        if count == 0 {
            // A size below 4 gives an empty list.
            return Vec::new();
        }
        let mut ids: Vec<AudioUnitParameterID> = vec![0; count as usize];
        let mut io_size: u32 = count * ID_BYTES;
        // SAFETY: `ids` owns exactly `count * 4` writable bytes, and `io_size` tells the unit exactly that
        // size, so it cannot write past the buffer; `io_size` is a valid u32 in/out pointer.
        let status = unsafe {
            AudioUnitGetProperty(
                unit,
                PROP_PARAMETER_LIST,
                SCOPE_GLOBAL,
                0,
                ids.as_mut_ptr().cast::<c_void>(),
                &mut io_size,
            )
        };
        if status != 0 {
            return Vec::new();
        }
        let returned = ((io_size / ID_BYTES) as usize).min(ids.len());
        ids.truncate(returned);
        ids
    }
}

/// A CFStringRef as text: non-null, a CFString, and UTF-8 convertible; otherwise "".
/// A successful conversion gives the bytes up to the first NUL, lossy UTF-8.
fn cf_text(s: CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: `s` is non-null and, per the AU contract, a CF object reference valid for this call.
    let is_string = unsafe { CFGetTypeID(s) == CFStringGetTypeID() };
    if !is_string {
        return String::new();
    }
    // SAFETY: `s` was just checked to be a CFString.
    let length = unsafe { CFStringGetLength(s) };
    // SAFETY: pure size query on a length and the crate's UTF-8 encoding constant.
    let max = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_UTF8) };
    if max < 0 {
        return String::new();
    }
    let Some(cap) = max.checked_add(1) else {
        return String::new();
    };
    let Ok(cap_usize) = usize::try_from(cap) else {
        return String::new();
    };
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(cap_usize).is_err() {
        return String::new();
    }
    buf.resize(cap_usize, 0);
    // SAFETY: `s` is a CFString; `buf` owns `cap` initialised, writable bytes and `cap` is passed as its size.
    let ok = unsafe { CFStringGetCString(s, buf.as_mut_ptr().cast::<c_char>(), cap, CF_UTF8) };
    if ok == 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The fixed `name` array: bytes up to the first NUL (all if none), each byte as is, lossy UTF-8.
fn fixed_name(name: &[c_char; 52]) -> String {
    let bytes: Vec<u8> = name
        .iter()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
