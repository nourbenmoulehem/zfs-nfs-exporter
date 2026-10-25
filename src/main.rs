

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::process:Command;

use tiny_http::{Header, Response, Server};

const LISTEN_ADDR: &str = "0.0.0.0:9877";
const ARCSTATS_PATH: &str = "/proc/spl/kstat/zfs/arcstats";
const NFSD_PATH: &str = "/proc/net/rpc/nfsd";


// "proc7ops" counter start by 0 representing an operation, operation read and write are consec. 25 and 38
const NFS4_OP_READ: usize = 25;
const NFS4_OP_WRITE: usize = 38;



/// struct as reported by `zpool list`
#[derive(Debug, PartialEq)]
struct Pool {
    name: String,
    size: u64,
    allocated: u64,
    free: u64,
    health: String,
}


/// metrics given back by /proc/net/rpc/nfsd
#[derive(Debug, Default, PartialEq)]
struct NfsStats {
    rpc_calls: u64,
    v4_reads: u64,
    v4_writes: u64,
}


/// Parses /proc/spl/kstat/zfs/arcstats.
fn parse_arcstats(text: &str) -> HashMap<String, u64> {
    let mut stats = HashMap::new();
    for line in text.lines().skip(2) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 3
            && let Ok(value) = fields[2].parse::<u64>()
        {
            stats.insert(fields[0].to_string(), value);
        }
    }
    stats
}



/// Parses the output of `zpool list -Hp -o name,size,allocated,free,health`.
fn parse_zpool_list(text: &str) -> Vec<Pool> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 5 {
                return None;
            }
            Some(Pool {
                name: f[0].to_string(),
                size: f[1].parse().ok()?,
                allocated: f[2].parse().ok()?,
                free: f[3].parse().ok()?,
                health: f[4].trim().to_string(),
            })
        })
        .collect()
}



/// Parses /proc/net/rpc/nfsd.
fn parse_nfsd(text: &str) -> NfsStats {
    let mut stats = NfsStats::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.first() {
            Some(&"rpc") => {
                stats.rpc_calls = fields.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            Some(&"proc4ops") => {
                // fields[0] = "proc4ops", fields[1] = number of ops, then op0, op1...
                let op = |n: usize| fields.get(2 + n).and_then(|v| v.parse().ok()).unwrap_or(0);
                stats.v4_reads = op(NFS4_OP_READ);
                stats.v4_writes = op(NFS4_OP_WRITE);
            }
            _ => {}
        }
    }
    stats
}



/// Appends one metric family (HELP + TYPE + samples) in Prometheus text format.
fn metric(out: &mut String, name: &str, kind: &str, help: &str, samples: &[(String, u64)]) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for (labels, value) in samples {
        let _ = writeln!(out, "{name}{labels} {value}");
    }
}


/// Builds the whole /metrics page from already-parsed data.
fn render(arc: Option<&HashMap<String, u64>>, pools: &[Pool], nfs: Option<&NfsStats>) -> String {
    let mut out = String::new();

    if let Some(arc) = arc {
        let get = |k: &str| arc.get(k).copied().unwrap_or(0);
        metric(&mut out, "zfs_arc_size_bytes", "gauge",
            "Current size of the ZFS ARC read cache.", &[(String::new(), get("size"))]);
        metric(&mut out, "zfs_arc_max_bytes", "gauge",
            "Maximum size of the ZFS ARC.", &[(String::new(), get("c_max"))]);
        metric(&mut out, "zfs_arc_hits_total", "counter",
            "Reads served from the ARC.", &[(String::new(), get("hits"))]);
        metric(&mut out, "zfs_arc_misses_total", "counter",
            "Reads that missed the ARC and went to disk.", &[(String::new(), get("misses"))]);
    }

    let by_pool = |f: fn(&Pool) -> u64| -> Vec<(String, u64)> {
        pools.iter().map(|p| (format!("{{pool=\"{}\"}}", p.name), f(p))).collect()
    };
    metric(&mut out, "zfs_pool_size_bytes", "gauge",
        "Total raw size of the pool.", &by_pool(|p| p.size));
    metric(&mut out, "zfs_pool_allocated_bytes", "gauge",
        "Space used in the pool.", &by_pool(|p| p.allocated));
    metric(&mut out, "zfs_pool_free_bytes", "gauge",
        "Free space in the pool.", &by_pool(|p| p.free));
    metric(&mut out, "zfs_pool_online", "gauge",
        "1 if the pool is ONLINE, 0 if DEGRADED, FAULTED or other.",
        &by_pool(|p| u64::from(p.health == "ONLINE")));
    let health: Vec<(String, u64)> = pools
        .iter()
        .map(|p| (format!("{{pool=\"{}\",state=\"{}\"}}", p.name, p.health), 1))
        .collect();
    metric(&mut out, "zfs_pool_health", "gauge",
        "Current pool state as a label (always 1).", &health);

    if let Some(nfs) = nfs {
        metric(&mut out, "nfsd_rpc_calls_total", "counter",
            "Total RPC calls received by the NFS server.", &[(String::new(), nfs.rpc_calls)]);
        metric(&mut out, "nfsd_v4_read_ops_total", "counter",
            "NFSv4 READ operations served.", &[(String::new(), nfs.v4_reads)]);
        metric(&mut out, "nfsd_v4_write_ops_total", "counter",
            "NFSv4 WRITE operations served.", &[(String::new(), nfs.v4_writes)]);
    }

    out
}


/// execute commands, parse, and render the metrics page
fn collect() -> String {

    let arc = fs::read_to_string(ARCSTATS_PATH).ok().map(|t| parse_arcstats(&t));


    let pools = Command::new("zpool")
        .args(["list", "-Hp", "-o", "name,size,allocated,free,health"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_zpool_list(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();


    let nfs = fs::read_to_string(NFSD_PATH).ok().map(|t| parse_nfsd(&t));


    render(arc.as_ref(), &pools, nfs.as_ref())
}


fn main() {

    let server = Server::http(LISTEN_ADDR).expect("could not bind the listen address");
    println!("zfs-nfs-exporter listening on http://{LISTEN_ADDR}/metrics");

    let content_type = Header::from_bytes("Content-Type", "text/plain; version=0.0.4").unwrap();

    for request in server.incoming_requests() {
        let response = if request.url() == "/metrics" {
            Response::from_string(collect()).with_header(content_type.clone())
        } else {
            Response::from_string("see /metrics\n").with_status_code(404)
        };
        let _ = request.respond(response);
    }
}


/// ********** UNIT TESTING ***************
#[cfg(test)]
mod tests {
    use super::*;

    const ARCSTATS: &str = "13 1 0x01 123 33456 1234 5678\nname                            type data\nhits                            4    1500\nmisses                          4    500\nsize                            4    104857600\nc_max                           4    1073741824\n";

    #[test]
    fn arcstats_skips_headers_and_reads_values() {
        let stats = parse_arcstats(ARCSTATS);
        assert_eq!(stats["hits"], 1500);
        assert_eq!(stats["misses"], 500);
        assert_eq!(stats["c_max"], 1_073_741_824);
        assert!(!stats.contains_key("name"));
    }

    #[test]
    fn zpool_list_parses_tab_separated_lines() {
        let pools = parse_zpool_list("tank\t16106127360\t4294967296\t11811160064\tDEGRADED\n");
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].name, "tank");
        assert_eq!(pools[0].allocated, 4_294_967_296);
        assert_eq!(pools[0].health, "DEGRADED");
    }

    #[test]
    fn nfsd_reads_rpc_and_v4_read_write() {
        // Build a proc4ops line with 40 counters, READ (25) = 50, WRITE (38) = 70.
        let mut ops = vec![0u64; 40];
        ops[NFS4_OP_READ] = 50;
        ops[NFS4_OP_WRITE] = 70;
        let ops: Vec<String> = ops.iter().map(|v| v.to_string()).collect();
        let text = format!("rc 0 12 300\nrpc 420 0 0 0 0\nproc4ops 40 {}\n", ops.join(" "));
        let stats = parse_nfsd(&text);
        assert_eq!(stats.rpc_calls, 420);
        assert_eq!(stats.v4_reads, 50);
        assert_eq!(stats.v4_writes, 70);
    }

    #[test]
    fn degraded_pool_is_not_online() {
        let pools = parse_zpool_list("tank\t1\t1\t0\tDEGRADED\n");
        let page = render(None, &pools, None);
        assert!(page.contains("zfs_pool_online{pool=\"tank\"} 0"));
        assert!(page.contains("zfs_pool_health{pool=\"tank\",state=\"DEGRADED\"} 1"));
    }
}

