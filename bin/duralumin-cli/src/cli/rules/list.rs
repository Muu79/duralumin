use anyhow::Result;
use comfy_table::{Cell, Color};
use duralumin_rules::config::{DynamicRuleConfig, DynamicRuleKind, RuleConfig, RuleKind};
use owo_colors::OwoColorize;

use crate::cli::helpers::make_table;
use crate::config;

pub fn cmd_rules_list(cfg: &config::Config) -> Result<()> {
    fn fmt_kind(k: &RuleKind) -> String {
        match k {
            RuleKind::Always => "always".into(),
            RuleKind::TitleRegex { pattern } => format!("title ~ /{pattern}/"),
            RuleKind::DescriptionRegex { pattern } => format!("description ~ /{pattern}/"),
            RuleKind::DurationMin { value } => {
                format!("duration >= {}", humantime::format_duration(*value))
            }
            RuleKind::DurationMax { value } => {
                format!("duration <= {}", humantime::format_duration(*value))
            }
            RuleKind::PublishedAfter { date } => {
                format!("published after {}", date.format("%Y-%m-%d"))
            }
            RuleKind::PublishedBefore { date } => {
                format!("published before {}", date.format("%Y-%m-%d"))
            }
            RuleKind::EpisodeSizeMax { value } => format!("size <= {value}"),
        }
    }

    fn fmt_dynamic_kind(dk: &DynamicRuleKind) -> String {
        match dk {
            DynamicRuleKind::DurationAgo { duration } => {
                format!(
                    "published within the last {}",
                    humantime::format_duration(duration.to_std().unwrap())
                )
            }
            DynamicRuleKind::LastNEpisodes { last_n_episodes: n } => {
                format!("among the {} most recent episodes", n)
            }
        }
    }

    fn rules_table(rules: &[RuleConfig]) -> comfy_table::Table {
        let mut t = make_table();
        t.set_header(["PRI", "NAME", "MATCH", "ACTION"]);
        for r in rules {
            let action_str = r.action.to_string();
            let action_cell = match action_str.as_str() {
                "download" => Cell::new(&action_str).fg(Color::Green),
                "skip" => Cell::new(&action_str).fg(Color::DarkGrey),
                _ => Cell::new(&action_str),
            };
            t.add_row([
                Cell::new(r.priority),
                Cell::new(&r.name),
                Cell::new(fmt_kind(&r.match_)),
                action_cell,
            ]);
        }
        t
    }

    fn dynamic_rules_table(dyn_rules: &[DynamicRuleConfig]) -> comfy_table::Table {
        let mut t = make_table();
        t.set_header(["NAME", "MATCH"]);
        for dr in dyn_rules {
            t.add_row([Cell::new(&dr.name), Cell::new(fmt_dynamic_kind(&dr.match_))]);
        }
        t
    }

    println!("{}", "Global Dynamic Rules".bold());
    if cfg.global_dynamics.is_empty() {
        println!("\t{}", "(none)".dimmed())
    } else {
        println!("{}", dynamic_rules_table(&cfg.global_dynamics))
    }

    println!("{}", "Global rules:".bold());
    if cfg.global_rules.is_empty() {
        println!("\t{}", "(none)".dimmed());
    } else {
        println!("{}", rules_table(&cfg.global_rules));
    }

    for feed in &cfg.feeds {
        println!();
        let feed_label = feed.display_name.as_deref().unwrap_or(&feed.slug);
        if feed.display_name.is_some() {
            println!(
                "{} {} {}",
                "Feed:".bold(),
                feed_label,
                format!("({})", feed.slug).dimmed()
            );
        } else {
            println!("{} {}", "Feed:".bold(), feed_label);
        }

        if feed.rules.is_empty() {
            println!("\t{}", "(none — global rules apply)".dimmed());
        } else {
            println!("{}", rules_table(&feed.rules));
        }

        println!("{}", "Dynamic rules:".bold());
        if feed.dynamic.is_empty() {
            println!("\t{}", "(none)".dimmed())
        } else {
            println!("{}", dynamic_rules_table(&feed.dynamic))
        }
        if let Some(action) = feed.default_action {
            println!("  {} {}", "catch-all:".dimmed(), action);
        }
    }

    println!();
    println!(
        "{} {}",
        "Default (no match):".dimmed(),
        cfg.defaults.action_on_no_match
    );
    Ok(())
}
