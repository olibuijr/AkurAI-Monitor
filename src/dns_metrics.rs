//! DNS metrics derived from the akurai-dns log stream.
//!
//! akurai-dns logs one structured line per query (`… UDP query … qtype=1
//! rcode=0 size=… elapsed_us=…`). The journald follower already ingests those
//! lines, so rather than bolt an HTTP metrics endpoint onto the DNS binary we
//! tap that stream here: `observe()` accumulates per-query counters into a
//! shared aggregator, and `drain()` (called once per persist interval, in
//! lockstep with the system-metric collector) turns the window into named
//! time-series the dashboard charts already know how to plot.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::collectors::Metric;

/// Sample cap per window — bounds memory if the server ever gets hammered.
/// Percentiles stay representative well below this.
const LAT_CAP: usize = 50_000;

#[derive(Default)]
struct Agg {
    udp: u64,
    tcp: u64,
    rcode: HashMap<u16, u64>,
    qtype: HashMap<u16, u64>,
    lat_us: Vec<u32>,
}

fn agg() -> &'static Mutex<Agg> {
    static AGG: OnceLock<Mutex<Agg>> = OnceLock::new();
    AGG.get_or_init(|| Mutex::new(Agg::default()))
}

/// Last computed window, so the fast SSE status push can surface DNS badges
/// between persist intervals without re-draining the aggregator.
fn latest_holder() -> &'static Mutex<Vec<Metric>> {
    static LATEST: OnceLock<Mutex<Vec<Metric>>> = OnceLock::new();
    LATEST.get_or_init(|| Mutex::new(zero_metrics(60)))
}

/// Feed one akurai-dns log line. Non-query lines are ignored.
pub fn observe(line: &str) {
    let is_tcp = if line.contains("UDP query") {
        false
    } else if line.contains("TCP query") {
        true
    } else {
        return;
    };

    let mut a = agg().lock().unwrap();
    if is_tcp {
        a.tcp += 1;
    } else {
        a.udp += 1;
    }
    if let Some(r) = field_u16(line, "rcode") {
        *a.rcode.entry(r).or_insert(0) += 1;
    }
    if let Some(t) = field_u16(line, "qtype") {
        *a.qtype.entry(t).or_insert(0) += 1;
    }
    if let Some(e) = field_u32(line, "elapsed_us")
        && a.lat_us.len() < LAT_CAP
    {
        a.lat_us.push(e);
    }
}

/// Drain the current window into metrics and update the live snapshot.
/// Called once per `interval_secs` so `dns.qps` reflects the true rate.
pub fn drain(interval_secs: u64) -> Vec<Metric> {
    let snapshot = std::mem::take(&mut *agg().lock().unwrap());
    let metrics = build_metrics(snapshot, interval_secs);
    *latest_holder().lock().unwrap() = metrics.clone();
    metrics
}

/// Last computed window (zeros until the first drain). Used by the fast SSE
/// push so DNS cards stay present and update each interval.
pub fn latest() -> Vec<Metric> {
    latest_holder().lock().unwrap().clone()
}

fn build_metrics(a: Agg, interval_secs: u64) -> Vec<Metric> {
    let interval = interval_secs.max(1) as f64;
    let total = a.udp + a.tcp;

    let mut out = vec![
        m("dns.qps", total as f64 / interval),
        m("dns.queries", total as f64),
        m("dns.proto.udp", a.udp as f64),
        m("dns.proto.tcp", a.tcp as f64),
    ];

    // Response codes — fixed buckets so the chart series are stable.
    let rc = |code: u16| *a.rcode.get(&code).unwrap_or(&0) as f64;
    let rc_known: u64 = [0u16, 1, 2, 3, 5]
        .iter()
        .map(|c| a.rcode.get(c).copied().unwrap_or(0))
        .sum();
    let rc_total: u64 = a.rcode.values().sum();
    out.push(m("dns.rcode.noerror", rc(0)));
    out.push(m("dns.rcode.formerr", rc(1)));
    out.push(m("dns.rcode.servfail", rc(2)));
    out.push(m("dns.rcode.nxdomain", rc(3)));
    out.push(m("dns.rcode.refused", rc(5)));
    out.push(m(
        "dns.rcode.other",
        rc_total.saturating_sub(rc_known) as f64,
    ));

    // Query types — common buckets + catch-all.
    let qt = |t: u16| *a.qtype.get(&t).unwrap_or(&0) as f64;
    let qt_known: u64 = [1u16, 28, 15, 16, 2, 6, 12, 5, 33, 257]
        .iter()
        .map(|t| a.qtype.get(t).copied().unwrap_or(0))
        .sum();
    let qt_total: u64 = a.qtype.values().sum();
    out.push(m("dns.qtype.a", qt(1)));
    out.push(m("dns.qtype.aaaa", qt(28)));
    out.push(m("dns.qtype.mx", qt(15)));
    out.push(m("dns.qtype.txt", qt(16)));
    out.push(m("dns.qtype.ns", qt(2)));
    out.push(m("dns.qtype.soa", qt(6)));
    out.push(m("dns.qtype.ptr", qt(12)));
    out.push(m("dns.qtype.cname", qt(5)));
    out.push(m("dns.qtype.srv", qt(33)));
    out.push(m("dns.qtype.caa", qt(257)));
    out.push(m(
        "dns.qtype.other",
        qt_total.saturating_sub(qt_known) as f64,
    ));

    // Latency (microseconds).
    let (avg, p95, max) = latency_stats(&a.lat_us);
    out.push(m("dns.latency_us.avg", avg));
    out.push(m("dns.latency_us.p95", p95));
    out.push(m("dns.latency_us.max", max));

    out
}

/// The full metric name set with zero values — seeds the live snapshot so DNS
/// cards/charts render immediately instead of waiting for the first query.
fn zero_metrics(interval_secs: u64) -> Vec<Metric> {
    build_metrics(Agg::default(), interval_secs)
}

fn latency_stats(samples: &[u32]) -> (f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut s: Vec<u32> = samples.to_vec();
    s.sort_unstable();
    let n = s.len();
    let avg = s.iter().map(|&x| x as f64).sum::<f64>() / n as f64;
    // Nearest-rank p95.
    let idx = (((n as f64) * 0.95).ceil() as usize).clamp(1, n) - 1;
    let p95 = s[idx] as f64;
    let max = s[n - 1] as f64;
    (avg.round(), p95, max)
}

fn m(name: &str, value: f64) -> Metric {
    Metric {
        name: name.to_string(),
        value,
    }
}

/// Extract a `key=<digits>` field value as u32. Requires the char before the
/// key to be a non-word boundary so `size=` can't match inside another token.
fn field_u32(line: &str, key: &str) -> Option<u32> {
    field_str(line, key).and_then(|s| s.parse().ok())
}
fn field_u16(line: &str, key: &str) -> Option<u16> {
    field_str(line, key).and_then(|s| s.parse().ok())
}

fn field_str<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    let pat = format!("{key}=");
    while let Some(rel) = line[from..].find(&pat) {
        let at = from + rel;
        let before_ok = at == 0
            || !line.as_bytes()[at - 1].is_ascii_alphanumeric() && line.as_bytes()[at - 1] != b'_';
        if before_ok {
            let vstart = at + pat.len();
            let rest = &line[vstart..];
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
        from = at + pat.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(metrics: &[Metric], name: &str) -> f64 {
        metrics.iter().find(|m| m.name == name).unwrap().value
    }

    #[test]
    fn parses_udp_query_line() {
        let line = "2026-06-25T16:16:29Z  INFO akurai_dns::server: UDP query client=127.0.0.1:49076 qname=example.com. qtype=1 rcode=0 size=54 elapsed_us=58";
        assert_eq!(field_u16(line, "qtype"), Some(1));
        assert_eq!(field_u16(line, "rcode"), Some(0));
        assert_eq!(field_u32(line, "elapsed_us"), Some(58));
        // "size" must not be confused with anything, and word-boundary holds.
        assert_eq!(field_u32(line, "size"), Some(54));
    }

    #[test]
    fn aggregates_window() {
        let mut a = Agg::default();
        // simulate observe by hand
        for (proto_tcp, qtype, rcode, lat) in [
            (false, 1u16, 0u16, 100u32),
            (false, 1, 0, 200),
            (true, 28, 3, 300),
            (false, 12, 5, 50),
        ] {
            if proto_tcp {
                a.tcp += 1
            } else {
                a.udp += 1
            }
            *a.qtype.entry(qtype).or_insert(0) += 1;
            *a.rcode.entry(rcode).or_insert(0) += 1;
            a.lat_us.push(lat);
        }
        let metrics = build_metrics(a, 60);
        assert_eq!(val(&metrics, "dns.queries"), 4.0);
        assert_eq!(val(&metrics, "dns.proto.udp"), 3.0);
        assert_eq!(val(&metrics, "dns.proto.tcp"), 1.0);
        assert_eq!(val(&metrics, "dns.qtype.a"), 2.0);
        assert_eq!(val(&metrics, "dns.qtype.ptr"), 1.0);
        assert_eq!(val(&metrics, "dns.rcode.noerror"), 2.0);
        assert_eq!(val(&metrics, "dns.rcode.refused"), 1.0);
        assert_eq!(val(&metrics, "dns.latency_us.max"), 300.0);
        assert!((val(&metrics, "dns.qps") - 4.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn ignores_non_query_lines() {
        let before = { agg().lock().unwrap().udp };
        observe(
            "2026-06-25T16:16:29Z  INFO akurai_dns::server: UDP server listening addr=0.0.0.0:53",
        );
        let after = { agg().lock().unwrap().udp };
        assert_eq!(before, after);
    }
}
