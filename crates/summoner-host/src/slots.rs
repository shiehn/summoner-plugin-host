//! What a slot's `PluginRef` (protocol 4.0's `SlotDesc.plugin`) becomes. Named first (the status record and the crash
//! attribution carry the name before the plugin is created), then made on the message thread inside the load's status
//! window, for the chain's channel counts. A slot that fails to load is a slot that is not ok and passes audio through.
use std::sync::Arc;
use summoner_proc::Plugin;
use summoner_proto::plugin::PluginRef;

/// What making a slot gets: the chain's channel counts.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub ch_in: u32,
    pub ch_out: u32,
}

/// Makes the slot's plugin, or says why it could not.
pub type Make = Box<dyn FnOnce(Context) -> Result<Arc<dyn Plugin>, String>>;

pub struct Described {
    pub name: String,
    /// The test double (its name is its own config's).
    pub test_double: bool,
    pub make: Make,
}

/// The slot's name: a test double's own name; otherwise the ref's display name when it has one, even when the load
/// then fails; else `fake<slot>`.
pub fn slot_name(r: &PluginRef, slot: usize, described: Option<&Described>) -> String {
    if let Some(d) = described.filter(|d| d.test_double) {
        return d.name.clone();
    }
    Some(r.name())
        .filter(|n| !n.is_empty())
        .map_or_else(|| format!("fake{slot}"), str::to_string)
}

/// `None`: not a slot this build hosts. A build without test hooks has no test double at all (the `summoner-testproc`
/// dependency is the feature's), so a test double's ref is no VST3 or AU ref and is `None`: a slot that is not ok.
pub fn describe(r: &PluginRef, slot: usize) -> Option<Described> {
    #[cfg(feature = "test-hooks")]
    if let Some(cfg) = summoner_testproc::Config::from_ref(r, slot) {
        return Some(Described {
            name: cfg.name.clone(),
            test_double: true,
            make: Box::new(move |_| Ok(summoner_testproc::make(cfg))),
        });
    }
    let _ = slot;
    // An AU v2 slot: a lab bundle by absolute path, the entry by its codes.
    if let Some(d) = summoner_format_au::desc::of_ref(r) {
        return Some(Described {
            name: d.name.clone(),
            test_double: false,
            make: Box::new(move |c| {
                summoner_format_au::load(&d, c.ch_in, c.ch_out)
                    .map(|p| Arc::new(p) as Arc<dyn Plugin>)
                    .map_err(String::from)
            }),
        });
    }
    // A real VST3 slot, loaded by its absolute path and class id; with a `--plugin-root`, only at
    // `<root>/vst3/<name>.vst3` (the lexical half, before anything is touched; the lstat and pin checks
    // follow in the load).
    let d = summoner_format_vst3::desc::of_ref(r)?;
    Some(Described {
        name: d.name.clone(),
        test_double: false,
        make: Box::new(move |c| {
            #[cfg(feature = "test-hooks")]
            if let Some(root) = PLUGIN_ROOT.get() {
                root.admits(&d.file)?;
            }
            summoner_format_vst3::load(&d, c.ch_in, c.ch_out)
                .map(|p| Arc::new(p) as Arc<dyn Plugin>)
                .map_err(String::from)
        }),
    })
}

/// The `--plugin-root` this process was started with (checked in main, before fd 3), if any.
#[cfg(feature = "test-hooks")]
static PLUGIN_ROOT: std::sync::OnceLock<summoner_sys::bundle::PluginRoot> =
    std::sync::OnceLock::new();

/// Called once, from main, before any slot exists.
#[cfg(feature = "test-hooks")]
pub fn set_plugin_root(root: summoner_sys::bundle::PluginRoot) {
    let _ = PLUGIN_ROOT.set(root);
}
