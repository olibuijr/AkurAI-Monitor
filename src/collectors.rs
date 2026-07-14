use std::collections::HashMap;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Metric {
    pub name: String,
    pub value: f64,
}

pub fn collect_all() -> Vec<Metric> {
    let mut metrics = Vec::new();
    metrics.extend(collect_cpu());
    metrics.extend(collect_memory());
    metrics.extend(collect_disk());
    metrics.extend(collect_load());
    metrics.extend(collect_uptime());
    metrics.extend(collect_network());
    metrics
}

fn collect_cpu() -> Vec<Metric> {
    let read_cpu = || -> Option<(u64, u64)> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let line = stat.lines().next()?;
        let parts: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|s| s.parse().ok())
            .collect();
        if parts.len() < 4 {
            return None;
        }
        let idle = parts[3];
        let total: u64 = parts.iter().sum();
        Some((idle, total))
    };

    let Some((idle1, total1)) = read_cpu() else {
        return vec![];
    };
    std::thread::sleep(std::time::Duration::from_secs(1));
    let Some((idle2, total2)) = read_cpu() else {
        return vec![];
    };

    let idle_delta = idle2.saturating_sub(idle1) as f64;
    let total_delta = total2.saturating_sub(total1) as f64;
    if total_delta == 0.0 {
        return vec![];
    }

    let usage = (1.0 - idle_delta / total_delta) * 100.0;
    vec![Metric {
        name: "cpu.usage".to_string(),
        value: (usage * 10.0).round() / 10.0,
    }]
}

fn collect_memory() -> Vec<Metric> {
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return vec![];
    };

    let mut values: HashMap<&str, u64> = HashMap::new();
    for line in meminfo.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let key = parts[0].trim_end_matches(':');
            if let Ok(val) = parts[1].parse::<u64>() {
                values.insert(key, val);
            }
        }
    }

    let total = *values.get("MemTotal").unwrap_or(&0);
    let available = *values.get("MemAvailable").unwrap_or(&0);
    if total == 0 {
        return vec![];
    }

    let used = total.saturating_sub(available);
    let used_pct = (used as f64 / total as f64) * 100.0;

    vec![
        Metric {
            name: "mem.total_kb".to_string(),
            value: total as f64,
        },
        Metric {
            name: "mem.used_kb".to_string(),
            value: used as f64,
        },
        Metric {
            name: "mem.available_kb".to_string(),
            value: available as f64,
        },
        Metric {
            name: "mem.used_pct".to_string(),
            value: (used_pct * 10.0).round() / 10.0,
        },
    ]
}

fn collect_disk() -> Vec<Metric> {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return vec![];
    };

    let mut metrics = Vec::new();
    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let mount = parts[1];
        let fstype = parts[2];

        // Only real filesystems
        if !matches!(fstype, "ext4" | "ext3" | "xfs" | "btrfs" | "zfs") {
            continue;
        }

        unsafe {
            let c_path = std::ffi::CString::new(mount).unwrap();
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
                continue;
            }

            let total = stat.f_blocks * stat.f_frsize as u64;
            let free = stat.f_bfree * stat.f_frsize as u64;
            if total == 0 {
                continue;
            }
            let used = total - free;
            let used_pct = (used as f64 / total as f64) * 100.0;

            let mount_label = mount.replace('/', ".");
            let mount_label = if mount_label == "." {
                ".root".to_string()
            } else {
                mount_label
            };

            metrics.push(Metric {
                name: format!("disk{mount_label}.used_pct"),
                value: (used_pct * 10.0).round() / 10.0,
            });
            metrics.push(Metric {
                name: format!("disk{mount_label}.total_gb"),
                value: (total as f64 / 1_073_741_824.0 * 10.0).round() / 10.0,
            });
            metrics.push(Metric {
                name: format!("disk{mount_label}.used_gb"),
                value: (used as f64 / 1_073_741_824.0 * 10.0).round() / 10.0,
            });
        }
    }

    metrics
}

fn collect_load() -> Vec<Metric> {
    let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") else {
        return vec![];
    };

    let parts: Vec<&str> = loadavg.split_whitespace().collect();
    if parts.len() < 3 {
        return vec![];
    }

    let mut metrics = Vec::new();
    if let Ok(v) = parts[0].parse::<f64>() {
        metrics.push(Metric {
            name: "load.1m".to_string(),
            value: v,
        });
    }
    if let Ok(v) = parts[1].parse::<f64>() {
        metrics.push(Metric {
            name: "load.5m".to_string(),
            value: v,
        });
    }
    if let Ok(v) = parts[2].parse::<f64>() {
        metrics.push(Metric {
            name: "load.15m".to_string(),
            value: v,
        });
    }

    metrics
}

fn collect_uptime() -> Vec<Metric> {
    let Ok(uptime) = std::fs::read_to_string("/proc/uptime") else {
        return vec![];
    };

    let parts: Vec<&str> = uptime.split_whitespace().collect();
    if let Some(Ok(secs)) = parts.first().map(|s| s.parse::<f64>()) {
        vec![Metric {
            name: "uptime.seconds".to_string(),
            value: secs.round(),
        }]
    } else {
        vec![]
    }
}

fn collect_network() -> Vec<Metric> {
    let read_net = || -> Option<HashMap<String, (u64, u64)>> {
        let dev = std::fs::read_to_string("/proc/net/dev").ok()?;
        let mut map = HashMap::new();
        for line in dev.lines().skip(2) {
            let line = line.trim();
            let (iface, rest) = line.split_once(':')?;
            let iface = iface.trim();
            if iface == "lo" {
                continue;
            }
            let vals: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if vals.len() >= 9 {
                map.insert(iface.to_string(), (vals[0], vals[8])); // rx_bytes, tx_bytes
            }
        }
        Some(map)
    };

    let Some(snap1) = read_net() else {
        return vec![];
    };
    // Sample over a 1s window so rx/tx deltas reflect actual throughput.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let Some(snap2) = read_net() else {
        return vec![];
    };

    let mut metrics = Vec::new();
    for (iface, (rx2, tx2)) in &snap2 {
        if let Some((rx1, tx1)) = snap1.get(iface) {
            metrics.push(Metric {
                name: format!("net.{iface}.rx_bytes"),
                value: rx2.saturating_sub(*rx1) as f64,
            });
            metrics.push(Metric {
                name: format!("net.{iface}.tx_bytes"),
                value: tx2.saturating_sub(*tx1) as f64,
            });
        }
    }

    metrics
}
