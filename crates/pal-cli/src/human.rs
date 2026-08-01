//! Terminal rendering. The JSON schema is the contract; this is for eyes.

use pal_analyze::query::*;
use pal_core::Evidence;

fn evidence_line(e: &Evidence) -> String {
    match e {
        Evidence::Structural {
            kind,
            resolution,
            direction,
            ..
        } => {
            let arrow = match direction {
                pal_core::Direction::Out => "->",
                pal_core::Direction::In => "<-",
            };
            format!("live {} {arrow} ({})", kind.as_str(), resolution.as_str())
        }
        Evidence::Ghost {
            kind,
            severed_at,
            severed_date,
            cochanges_since,
            confidence_since,
            severing_subject,
            ..
        } => format!(
            "GHOST {}: severed {severed_at} {severed_date} \"{severing_subject}\"; co-changed {cochanges_since}x since (conf {confidence_since:.2})",
            kind.as_str()
        ),
        Evidence::Cochange {
            n,
            confidence,
            lift,
            last,
            ..
        } => format!("co-change {n}x, conf {confidence:.2}, lift {lift:.1}, last {last}"),
        Evidence::Transitive {
            via,
            union_dist,
            head_dist,
        } => {
            let hd = head_dist
                .map(|d| d.to_string())
                .unwrap_or_else(|| "unreachable".into());
            if via.is_empty() {
                format!("transitive: union-dist {union_dist}, live-dist {hd}")
            } else {
                format!(
                    "transitive via {}: union-dist {union_dist}, live-dist {hd}",
                    via.join(" > ")
                )
            }
        }
        Evidence::DocDrift {
            commits_behind, ..
        } => format!("doc is {commits_behind} commits behind this file"),
    }
}

pub fn blast(out: &BlastOutput) {
    println!(
        "blast radius of {} (head {})",
        out.query.file, out.query.head
    );
    if out.results.is_empty() {
        println!("  nothing above threshold; absence of evidence is not evidence of absence");
    }
    for r in &out.results {
        println!("  {:.2}  {}", r.rank, r.path);
        for e in &r.evidence {
            println!("        {}", evidence_line(e));
        }
    }
    if out.truncated {
        println!("  (truncated; raise --limit for more)");
    }
    for c in &out.caveats {
        println!("  note: {c}");
    }
}

pub fn ghosts(out: &GhostsOutput) {
    if out.results.is_empty() {
        println!("no ghost edges");
        return;
    }
    for g in &out.results {
        println!("{:.2}  {} -> {} ({})", g.score, g.from, g.to, g.kind);
        println!(
            "      severed {} {} \"{}\"; lived {}d; co-changed {}x since (conf {:.2})",
            g.severed_at.oid,
            g.severed_at.date,
            g.severed_at.subject,
            g.lifetime_days,
            g.cochanges_since,
            g.confidence_since
        );
    }
    if out.truncated {
        println!("(truncated; raise --limit for more)");
    }
}

pub fn cochange(out: &CochangeOutput) {
    println!("evolutionary coupling for {}", out.query.file);
    if out.results.is_empty() {
        println!("  none above gates (n >= 3, lift >= 1.5)");
    }
    for r in &out.results {
        let tag = if r.structural_edge {
            " [structural]"
        } else {
            ""
        };
        println!(
            "  {}x conf {:.2} lift {:.1}  {}{tag}  ({} .. {})",
            r.n, r.confidence, r.lift, r.path, r.first, r.last
        );
    }
    if out.truncated {
        println!("  (truncated; raise --limit for more)");
    }
}

fn hops(hops: &[Hop]) {
    for h in hops {
        let state = if h.alive {
            "live".to_string()
        } else {
            match &h.died {
                Some(d) => format!("died {} {}", d.oid, d.date),
                None => "dead".to_string(),
            }
        };
        println!("  {} -> {} ({}, {state})", h.from, h.to, h.kind);
    }
}

pub fn path(out: &PathOutput) {
    match (&out.live_path, &out.union_path) {
        (Some(p), _) => {
            println!("live path {} .. {}:", out.a, out.b);
            hops(p);
        }
        (None, Some(p)) => {
            println!("no live path {} .. {}; historical path:", out.a, out.b);
            hops(p);
            if let Some(broke) = &out.broke_at {
                println!(
                    "  broke at {} {} \"{}\"",
                    broke.oid, broke.date, broke.subject
                );
            }
        }
        (None, None) => println!("no recorded path between {} and {}", out.a, out.b),
    }
}

pub fn why(out: &WhyOutput) {
    println!("{} and {}", out.a, out.b);
    for e in &out.edges {
        let state = if e.alive { "live" } else { "dead" };
        println!(
            "  {} edge {} -> {} ({}, {state})",
            e.kind, e.from, e.to, e.resolution
        );
        for iv in &e.intervals {
            match &iv.died {
                Some(d) => println!(
                    "    {} {} .. {} {} \"{}\"",
                    iv.born.oid, iv.born.date, d.oid, d.date, d.subject
                ),
                None => println!("    {} {} .. now", iv.born.oid, iv.born.date),
            }
        }
        if let Some(g) = &e.ghost {
            println!(
                "    GHOST: severed by {} ({}) \"{}\"; lived {}d; co-changed {}x since (conf {:.2}, score {:.2})",
                g.severed_at.oid,
                g.severing_author,
                g.severed_at.subject,
                g.lifetime_days,
                g.cochanges_since,
                g.confidence_since,
                g.score
            );
            if !g.severing_body.is_empty() {
                for line in g.severing_body.lines().take(4) {
                    println!("      | {line}");
                }
            }
        }
    }
    if let Some(c) = &out.cochange {
        println!(
            "  co-change: {}x, conf {:.2}, lift {:.1}, {} .. {}",
            c.n, c.confidence, c.lift, c.first, c.last
        );
    }
    for c in &out.caveats {
        println!("  note: {c}");
    }
}

pub fn timeline(out: &TimelineOutput) {
    println!("{}", out.file);
    println!(
        "  born {} {} \"{}\"",
        out.born.oid, out.born.date, out.born.subject
    );
    if let Some(d) = &out.died {
        println!("  died {} {} \"{}\"", d.oid, d.date, d.subject);
    }
    if out.paths.len() > 1 {
        println!("  paths:");
        for p in &out.paths {
            let conf = if p.confidence < 1.0 {
                format!(" (rename confidence {:.2})", p.confidence)
            } else {
                String::new()
            };
            println!("    {} from {} {}{conf}", p.path, p.from.oid, p.from.date);
        }
    }
    if !out.edge_events.is_empty() {
        println!("  edge events:");
        for e in &out.edge_events {
            let arrow = if e.direction == "out" { "->" } else { "<-" };
            println!(
                "    {} {} {} {} {arrow} {}",
                e.commit.date, e.commit.oid, e.event, e.kind, e.other
            );
        }
    }
    if !out.churn.is_empty() {
        println!("  churn:");
        for c in &out.churn {
            println!("    {}  {} touches, {} lines", c.period, c.touches, c.lines);
        }
    }
}

pub fn drift(out: &DriftOutput) {
    if out.results.is_empty() {
        println!("no doc drift detected");
        return;
    }
    for d in &out.results {
        println!(
            "{} commits behind: {} (last {}) references {} (last {})",
            d.commits_behind, d.doc, d.doc_last_touched, d.code, d.code_last_touched
        );
    }
    if out.truncated {
        println!("(truncated; raise --limit for more)");
    }
}

pub fn hotspots(out: &HotspotsOutput) {
    println!("hotspots by {}", out.by);
    for h in &out.results {
        println!("  {:>8.2}  {}  ({})", h.score, h.path, h.detail);
    }
    if out.truncated {
        println!("  (truncated; raise --limit for more)");
    }
}

pub fn search(out: &SearchOutput) {
    for r in &out.results {
        println!(
            "{} {} {} ({})",
            r.commit.oid, r.commit.date, r.commit.subject, r.author
        );
    }
}

pub fn stats(out: &StatsOutput) {
    println!(
        "head {} indexed_at {}",
        out.head,
        out.indexed_at.as_deref().unwrap_or("?")
    );
    println!(
        "commits: {} total, {} too-large, {} mechanical, {} merges, {}% excluded from co-change",
        out.commits.total,
        out.commits.excluded_too_large,
        out.commits.excluded_mechanical,
        out.commits.merges,
        out.commits.excluded_pct
    );
    println!(
        "files: {} total ({} alive, {} dead, {} docs)",
        out.files.total, out.files.alive, out.files.dead, out.files.docs
    );
    println!(
        "edges: {} total, {} live, {} ghosts; cochange pairs: {}",
        out.edges.total, out.edges.live, out.edges.ghosts, out.cochange_pairs
    );
    println!(
        "imports: {}/{} resolved (unresolved ratio {})",
        out.imports.resolved, out.imports.total, out.imports.unresolved_ratio
    );
    println!(
        "renames: {} total, {} low-confidence",
        out.renames.total, out.renames.low_confidence
    );
}
