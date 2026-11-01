//! The right-hand panel. It always answers the question the current screen raises: what is
//! this partition, what will this plan cost, what is protecting me, what just happened.

use bootshift_core::units::{Sectors, format_bytes};
use eframe::egui::{Pos2, Rect, Sense, Ui, Vec2};

use crate::app::{App, Goal, Screen};
use crate::editing::min_len;
use crate::format::{rough_time, short_name, size, thousands};
use crate::theme::{self, Face, Weight};
use crate::widgets::{self, Tone, block_gap, facts, readout_heading, readout_text, readout_texts};

/// Sequential read-write speed used only to turn bytes into a rough time for the user.
const BYTES_PER_SECOND: u64 = 450 << 20;

pub fn show(ui: &mut Ui, app: &App) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    match app.screen {
        Screen::Home => home(ui, app),
        Screen::Arrange => match app.goal {
            Some(goal) if goal != Goal::DoItMyself => goal_readout(ui, app, goal),
            _ => arrange(ui, app),
        },
        Screen::Plan => plan(ui, app),
        Screen::Checks => checks(ui, app),
        Screen::Done => done(ui, app),
        Screen::Migrate => {}
    }
}

/// On the home screen the readout shows the disk being worked on, so the choice of goal has
/// context.
fn home(ui: &mut Ui, app: &App) {
    let Some(probed) = &app.probed else {
        readout_heading(ui, "Disk");
        readout_text(ui, "Reading the disk…");
        return;
    };
    readout_heading(ui, "This disk");
    readout_text(ui, &probed.disk);
    readout_texts(
        ui,
        &[
            "Pick a goal on the left, and Kitten builds the change for you.",
            "Nothing touches the disk until you have seen the plan and the checks pass.",
        ],
    );
}

fn arrange(ui: &mut Ui, app: &App) {
    let Some(probed) = &app.probed else {
        readout_heading(ui, "Disk");
        readout_text(ui, "Reading the disk…");
        return;
    };
    let layout = &probed.layout;
    let partition = app.selected.and_then(|guid| layout.partition(guid));
    let Some(partition) = partition else {
        let taken: u64 = app.target.values().map(|e| e.len.0).sum();
        let free = Sectors(layout.geometry.usable.len.0.saturating_sub(taken));
        readout_heading(ui, "This disk");
        facts(
            ui,
            &[
                ("Device", probed.disk.clone(), Tone::Plain),
                ("Capacity", format_bytes(probed.size), Tone::Plain),
                ("Partition table", "GPT".to_string(), Tone::Plain),
                (
                    "Partitions",
                    layout.partitions.len().to_string(),
                    Tone::Plain,
                ),
                ("Unallocated", size(layout, free), Tone::Plain),
                (
                    "Alignment",
                    format!("{} boundaries", size(layout, layout.geometry.alignment)),
                    Tone::Plain,
                ),
            ],
        );
        block_gap(ui);
        readout_heading(ui, "Pick a partition");
        readout_text(
            ui,
            "Click a partition in the bar or the table to see what it holds and how far it can go.",
        );
        return;
    };

    let extent = app
        .target
        .get(&partition.unique_guid)
        .copied()
        .unwrap_or(partition.extent);
    let changed = |differs: bool| if differs { Tone::Changed } else { Tone::Plain };
    partition_title(
        ui,
        theme::content_colour(partition.content),
        &short_name(partition),
    );
    facts(
        ui,
        &[
            (
                "Contents",
                partition.content.name().to_string(),
                Tone::Plain,
            ),
            (
                "Partition number",
                partition.number.to_string(),
                Tone::Plain,
            ),
        ],
    );
    block_gap(ui);
    readout_heading(ui, "Space");
    let used_share = (partition.used.0 as f32 / extent.len.0.max(1) as f32).min(1.0);
    meter(ui, used_share);
    facts(
        ui,
        &[
            ("Used", size(layout, partition.used), Tone::Plain),
            ("Size now", size(layout, partition.extent.len), Tone::Plain),
            (
                "Size after",
                size(layout, extent.len),
                changed(extent.len != partition.extent.len),
            ),
            (
                "Smallest it can be",
                size(layout, min_len(layout, partition)),
                Tone::Plain,
            ),
        ],
    );
    block_gap(ui);
    readout_heading(ui, "Position");
    let moves = extent.start != partition.extent.start;
    let copy = if moves {
        size(layout, partition.used.min(extent.len))
    } else {
        "none".to_string()
    };
    facts(
        ui,
        &[
            (
                "Starts now",
                format!("sector {}", thousands(partition.extent.start.0)),
                Tone::Plain,
            ),
            (
                "Starts after",
                format!("sector {}", thousands(extent.start.0)),
                changed(moves),
            ),
            ("Data to copy", copy, changed(moves)),
        ],
    );
}

/// The readout for a simplified goal screen: plain before-and-after, no sector positions.
fn goal_readout(ui: &mut Ui, app: &App, goal: Goal) {
    let Some(probed) = &app.probed else {
        readout_heading(ui, "Disk");
        readout_text(ui, "Reading the disk…");
        return;
    };
    let layout = &probed.layout;
    readout_heading(ui, "Your change");
    if goal == Goal::CleanUp {
        let removed: Vec<_> = layout
            .partitions
            .iter()
            .filter(|p| app.is_deleted(p.unique_guid))
            .collect();
        if removed.is_empty() {
            readout_text(
                ui,
                "Click a partition to remove it; its space becomes free.",
            );
        } else {
            let freed: u64 = removed.iter().map(|p| p.extent.len.0).sum();
            readout_text(
                ui,
                &format!(
                    "{} to remove, freeing {}.",
                    removed.len(),
                    size(layout, Sectors(freed))
                ),
            );
        }
        block_gap(ui);
        readout_heading(ui, "What protects you");
        readout_texts(
            ui,
            &[
                "Only the table entry goes; the data stays on the disk until something writes over it, so a slip can be undone.",
                "Nothing is written until you've seen the plan and the checks pass.",
            ],
        );
        return;
    }
    let Some(guid) = app.goal_partition else {
        readout_text(ui, "Pick a partition on the left, then choose how much.");
        return;
    };
    let Some(partition) = layout.partition(guid) else {
        return;
    };
    let before = partition.extent;
    let after = app.target.get(&guid).copied().unwrap_or(before);
    partition_title(
        ui,
        theme::content_colour(partition.content),
        &short_name(partition),
    );
    let changed = |differs: bool| if differs { Tone::Changed } else { Tone::Plain };

    if after == before && goal != Goal::MovePartition {
        readout_text(ui, "Choose how much on the left and this fills in.");
    } else {
        match goal {
            Goal::GiveSpace => {
                let grew = after.len.0.saturating_sub(before.len.0);
                facts(
                    ui,
                    &[
                        ("Size now", size(layout, before.len), Tone::Plain),
                        ("Size after", size(layout, after.len), changed(grew > 0)),
                        ("Bigger by", size(layout, Sectors(grew)), changed(grew > 0)),
                    ],
                );
            }
            Goal::FreeSpace | Goal::InstallOs => {
                let freed = before.len.0.saturating_sub(after.len.0);
                let freed_label = if goal == Goal::InstallOs {
                    "Room for the new OS"
                } else {
                    "Freed up"
                };
                facts(
                    ui,
                    &[
                        ("Size now", size(layout, before.len), Tone::Plain),
                        ("Size after", size(layout, after.len), changed(freed > 0)),
                        (
                            freed_label,
                            size(layout, Sectors(freed)),
                            changed(freed > 0),
                        ),
                    ],
                );
            }
            Goal::MovePartition => {
                let moves = after.start != before.start;
                let toward = if app.goal_move_left {
                    "toward the start"
                } else {
                    "toward the end"
                };
                facts(
                    ui,
                    &[
                        ("Size", size(layout, before.len), Tone::Plain),
                        ("Moving", toward.to_string(), changed(moves)),
                        (
                            "Data to copy",
                            size(layout, partition.used.min(after.len)),
                            changed(moves),
                        ),
                    ],
                );
            }
            Goal::CleanUp | Goal::Migrate | Goal::DoItMyself => {}
        }
    }
    block_gap(ui);
    readout_heading(ui, "What protects you");
    readout_texts(
        ui,
        &[
            "Nothing is written until you have seen the plan and every safety check passes.",
            "Data is copied and verified in its new place before the partition table changes, so a power cut can't lose it.",
        ],
    );
}

fn partition_title(ui: &mut Ui, colour: eframe::egui::Color32, name: &str) {
    let galley = widgets::galley(ui, name, Face::Sans(Weight::SemiBold), 14.3, theme::INK);
    let height = galley.size().y;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter();
    let swatch = Rect::from_center_size(
        Pos2::new(rect.left() + 5.0, rect.center().y),
        Vec2::splat(10.0),
    );
    painter.rect_filled(swatch, 3, colour);
    widgets::text_at(
        painter,
        eframe::egui::Align2::LEFT_CENTER,
        Pos2::new(swatch.right() + 7.5, rect.center().y),
        galley,
    );
    ui.add_space(7.5);
}

fn meter(ui: &mut Ui, share: f32) {
    ui.add_space(8.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3, theme::RULE);
    let fill = Rect::from_min_size(rect.min, Vec2::new(rect.width() * share, rect.height()));
    painter
        .with_clip_rect(fill)
        .rect_filled(rect, 3, theme::SIGNAL);
    ui.add_space(5.0);
}

fn plan(ui: &mut Ui, app: &App) {
    let steps = app.plan.as_ref().map_or(0, |p| p.operations.len());
    let bytes = app.plan.as_ref().map_or(0, |p| p.copy_bytes);
    if steps == 0 && bytes == 0 {
        readout_heading(ui, "Nothing to do");
        readout_text(
            ui,
            "The layout matches the disk. Move something on the arrange screen and the plan fills in.",
        );
        return;
    }
    readout_heading(ui, "What this costs");
    facts(
        ui,
        &[
            ("Steps", steps.to_string(), Tone::Plain),
            ("Data copied", format_bytes(bytes), Tone::Plain),
            ("Chunk size", "64 MiB".to_string(), Tone::Plain),
            ("Restarts", "1".to_string(), Tone::Plain),
            (
                "Rough time",
                rough_time(bytes, BYTES_PER_SECOND),
                Tone::Plain,
            ),
        ],
    );
    block_gap(ui);
    readout_heading(ui, "What protects you");
    readout_texts(
        ui,
        &[
            "Data is copied and verified in its new place before the partition table is touched.",
            "A chunk is recorded only after the drive confirms the write, and only one write is in flight at a time.",
            "The journal on the EFI partition is the record of truth, so a power cut resumes from the last verified chunk.",
        ],
    );
}

fn checks(ui: &mut Ui, app: &App) {
    let total = app.checks.len();
    let passed = app.checks.iter().filter(|c| c.passed).count();
    let tally = if total == 0 {
        "—".to_string()
    } else {
        format!("{passed} of {total}")
    };
    let verdict = if total == 0 {
        "running"
    } else if app.checks_passed {
        "safe to start"
    } else {
        "refused"
    };
    readout_heading(ui, "Checks");
    facts(
        ui,
        &[
            (
                "Passed",
                tally,
                if app.checks_passed {
                    Tone::Good
                } else {
                    Tone::Plain
                },
            ),
            ("Verdict", verdict.to_string(), Tone::Plain),
        ],
    );
    block_gap(ui);
    readout_heading(ui, "What applying does");
    readout_texts(
        ui,
        &[
            "Kitten copies its recovery system onto the EFI partition and points the next boot at it. Your disk is not touched yet.",
            "The work happens during the restart you choose, before any operating system starts.",
        ],
    );
}

fn done(ui: &mut Ui, app: &App) {
    readout_heading(ui, "Kept on the EFI partition");
    facts(
        ui,
        &[
            ("Partition table backup", "saved".to_string(), Tone::Plain),
            ("Move journal", "saved".to_string(), Tone::Plain),
            ("Recovery boot redirect", "removed".to_string(), Tone::Plain),
        ],
    );
    block_gap(ui);
    readout_heading(ui, "If something looks wrong");
    readout_text(
        ui,
        if app.is_demo() {
            "In the real app, undo runs the same plan in reverse while the freed space is still empty."
        } else {
            "Undo runs the same plan in reverse while the freed space is still empty."
        },
    );
}
