use crate::model::{InstanceRecord, format_uptime};

pub fn print_table(records: &[InstanceRecord], with_cpu: bool) {
    if records.is_empty() {
        println!("No EC2 instances matched.");
        return;
    }

    let rows: Vec<Row> = records.iter().map(Row::from_record).collect();

    let headers: Vec<&'static str> = if with_cpu {
        vec![
            "instance_id",
            "state",
            "type",
            "uptime",
            "launched_by",
            "src",
            "region",
            "avg_cpu",
            "max_cpu",
        ]
    } else {
        vec![
            "instance_id",
            "state",
            "type",
            "uptime",
            "launched_by",
            "src",
            "region",
        ]
    };

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, col) in row.columns(with_cpu).iter().enumerate() {
            if col.len() > widths[i] {
                widths[i] = col.len();
            }
        }
    }

    print_row(&headers, &widths);
    print_separator(&widths);
    for row in &rows {
        print_row(&row.columns(with_cpu), &widths);
    }
}

struct Row {
    instance_id: String,
    state: String,
    instance_type: String,
    uptime: String,
    launched_by: String,
    source: String,
    region: String,
    avg_cpu: String,
    max_cpu: String,
}

impl Row {
    fn from_record(r: &InstanceRecord) -> Self {
        let (avg_cpu, max_cpu) = r
            .cpu
            .as_ref()
            .map(|c| (format_pct(c.avg_pct), format_pct(c.max_pct)))
            .unwrap_or_else(|| ("-".to_string(), "-".to_string()));

        Row {
            instance_id: r.instance_id.clone(),
            state: r.state.as_str().to_string(),
            instance_type: r.instance_type.clone(),
            uptime: format_uptime(r.uptime_seconds),
            launched_by: r.launched_by.clone().unwrap_or_else(|| "unknown".to_string()),
            source: r.launched_by_source.as_str().to_string(),
            region: r.region.clone(),
            avg_cpu,
            max_cpu,
        }
    }

    fn columns(&self, with_cpu: bool) -> Vec<&str> {
        let mut v = vec![
            self.instance_id.as_str(),
            self.state.as_str(),
            self.instance_type.as_str(),
            self.uptime.as_str(),
            self.launched_by.as_str(),
            self.source.as_str(),
            self.region.as_str(),
        ];
        if with_cpu {
            v.push(self.avg_cpu.as_str());
            v.push(self.max_cpu.as_str());
        }
        v
    }
}

fn format_pct(pct: Option<f64>) -> String {
    match pct {
        Some(v) => format!("{v:.1}%"),
        None => "-".to_string(),
    }
}

fn print_row<S: AsRef<str>>(cols: &[S], widths: &[usize]) {
    let mut line = String::new();
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        let s = col.as_ref();
        line.push_str(s);
        if i + 1 < cols.len() {
            for _ in s.len()..widths[i] {
                line.push(' ');
            }
        }
    }
    println!("{line}");
}

fn print_separator(widths: &[usize]) {
    let mut line = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        for _ in 0..*w {
            line.push('-');
        }
    }
    println!("{line}");
}
