//! Scan one plugin bundle in a process of its own. The scan supervisor spawns the host program in scan mode, in its
//! own process group, under a time budget, and turns whatever happens (a report, a crash, a hang, a protocol fault)
//! into a verdict. The pedal is where your application records a scan as pending before the spawn, so a scan that
//! takes the whole application down is still known on the next start.
//!
//! ```sh
//! cargo build -p summoner-host
//! cargo run -p summoner-scan --example summoner_scan_quickstart -- target/debug/summoner-host /Library/Audio/Plug-Ins/VST3/X.vst3
//! ```
use summoner_proto::codec::{Scan, ScanFormat};
use summoner_scan::scan::{Pedal, ScanConfig, ScanReport, ScanSupervisor};

/// A pedal that keeps its marks in memory. A real one writes them durably before it returns.
#[derive(Default)]
struct MemoryPedal {
    pending: Vec<String>,
}

impl Pedal for MemoryPedal {
    fn pending(&mut self, scan: &Scan) -> std::io::Result<()> {
        self.pending.push(scan.path().to_string());
        Ok(())
    }

    fn settle(&mut self, report: &ScanReport) -> std::io::Result<()> {
        self.pending.retain(|p| p != report.scan.path());
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(program), Some(bundle)) = (args.next(), args.next()) else {
        eprintln!("usage: summoner_scan_quickstart <host program> <bundle.vst3>");
        return Ok(());
    };
    let mut supervisor = ScanSupervisor::new(ScanConfig::new(program.into(), vec![]));
    let mut pedal = MemoryPedal::default();

    // The request is checked when it is made: an absolute bundle path of the named format.
    let scan = Scan::new(ScanFormat::Vst3, &bundle)?;
    let report = supervisor.scan(&scan, &mut pedal);

    println!(
        "{bundle}: {:?} in {:?}",
        report.end.verdict(),
        report.elapsed
    );
    if report.end.is_strike() {
        println!("a strike: the bundle crashed, hung or misbehaved during its scan");
    }
    for class in &report.discovered {
        println!("  found {}", class.name());
    }
    assert!(pedal.pending.is_empty(), "every started scan is settled");
    Ok(())
}
