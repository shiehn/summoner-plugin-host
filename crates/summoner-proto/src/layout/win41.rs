//! Protocol 4.1's Windows-only additive layout: **owned by the Windows port**. Empty until it is written; never built
//! on unix (the unix layout, its reserved words and its layout tests are unchanged).
//!
//! What goes here: the offsets of the FanOut registration masks (one `u64` per ring group, in the child page's
//! Windows-only region), and their const asserts.
