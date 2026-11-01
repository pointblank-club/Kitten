//! The whole app driven the way a person drives it: pointer drags, clicks and keys against the
//! demo backend. Set `BOOTSHIFT_SNAPSHOTS=<dir>` to also save each stage as a PNG.

use bootshift_core::layout::{Extent, Guid};
use bootshift_core::plan::Operation;
use bootshift_core::units::Sectors;
use eframe::egui::{Key, Modifiers, Pos2, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

use crate::app::{App, Goal, Screen};
use crate::backend::Worker;
use crate::demo::{DemoBackend, demo_layout};

const STEP: f32 = 0.05;
const GIB: u64 = 2048 * 1024;
const MIB: u64 = 2048;

fn harness(width: f32) -> Harness<'static, App> {
    let mut harness = Harness::builder()
        .with_size(Vec2::new(width, 900.0))
        .with_step_dt(STEP)
        .with_max_steps(2_000)
        .build_ui_state(
            |ui, app: &mut App| app.frame(ui),
            App::new(Worker::inline(Box::new(DemoBackend::new()))),
        );
    harness.run_steps(3);
    // The app opens on the home screen; the flow tests start in the editor, so enter it and let
    // the screen settle before the test's first interaction.
    harness.get_by_label("Do it myself").click();
    harness.run_steps(6);
    harness
}

fn guid(number: u32) -> Guid {
    demo_layout().partitions[number as usize - 1].unique_guid
}

/// A harness left on the home screen, before any goal is chosen.
fn home_harness(width: f32) -> Harness<'static, App> {
    let mut harness = Harness::builder()
        .with_size(Vec2::new(width, 900.0))
        .with_step_dt(STEP)
        .with_max_steps(2_000)
        .build_ui_state(
            |ui, app: &mut App| app.frame(ui),
            App::new(Worker::inline(Box::new(DemoBackend::new()))),
        );
    harness.run_steps(3);
    harness
}

fn seconds(harness: &mut Harness<'_, App>, seconds: f32) {
    harness.run_steps((seconds / STEP).ceil() as usize);
}

/// Presses at `from`, moves to `to` in a few pointer moves, and lets go.
fn drag(harness: &mut Harness<'_, App>, from: Pos2, to: Pos2) {
    harness.drag_at(from);
    harness.step();
    for share in [0.25, 0.5, 0.75, 1.0] {
        harness.hover_at(from + (to - from) * share);
        harness.step();
    }
    harness.drop_at(to);
    harness.step();
}

/// The number of points that `sectors` covers on the editable bar, measured on swap, which no
/// sliver overlaps.
fn points(harness: &Harness<'_, App>, sectors: u64) -> f32 {
    let swap = harness
        .get_by_label("swap: drag to move, drag edges to resize")
        .rect();
    let len = demo_layout().partitions[5].extent.len.0;
    swap.width() * sectors as f32 / len as f32
}

fn snapshot(harness: &mut Harness<'_, App>, name: &str) {
    let Ok(dir) = std::env::var("BOOTSHIFT_SNAPSHOTS") else {
        return;
    };
    let image = harness.render().expect("rendering the frame");
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    image.save(&path).expect("saving the snapshot");
}

fn extent(harness: &Harness<'_, App>, number: u32) -> Extent {
    harness.state().target[&guid(number)]
}

/// Shrink Windows by its right edge, slide the recovery sliver left, then slide Linux left:
/// the demo story that copies about 90 GiB.
fn arrange_the_demo_story(harness: &mut Harness<'_, App>) {
    let shift = points(harness, 83 * GIB + 512 * MIB);
    let edge = harness
        .get_by_label("Resize the end of Windows")
        .rect()
        .center();
    drag(harness, edge, edge - Vec2::new(shift, 0.0));
    let windows = extent(harness, 3);
    let wanted = 116 * GIB + 512 * MIB;
    assert!(
        windows.len.0.abs_diff(wanted) <= MIB,
        "Windows is {:?}",
        windows.len
    );

    let recovery = harness
        .get_by_label("Windows recovery: drag to move, drag edges to resize")
        .rect()
        .center();
    drag(harness, recovery, recovery - Vec2::new(shift, 0.0));
    assert_eq!(
        extent(harness, 4).start,
        windows.end(),
        "stops against Windows"
    );

    let linux = harness
        .get_by_label("Linux: drag to move, drag edges to resize")
        .rect()
        .center();
    drag(harness, linux, linux - Vec2::new(shift, 0.0));
    assert_eq!(
        extent(harness, 5).start,
        extent(harness, 4).end(),
        "stops against recovery"
    );
}

#[test]
fn home_screen_lists_the_goals() {
    let mut harness = home_harness(1440.0);
    assert_eq!(harness.state().screen, Screen::Home);
    harness.get_by_label("Install an operating system");
    harness.get_by_label("Do it myself");
    snapshot(&mut harness, "00-home");
    harness.get_by_label("Install an operating system").click();
    harness.run_steps(4);
    assert_eq!(harness.state().screen, Screen::Arrange);
    assert_eq!(harness.state().goal, Some(Goal::InstallOs));
    snapshot(&mut harness, "01-goal-install");
}

#[test]
fn resizing_an_edge_pushes_the_neighbour_and_hover_magnifies() {
    let mut harness = harness(1440.0);
    // Drag Windows' right edge outward; the recovery sliver and everything past it slide right.
    let before = extent(&harness, 4).start;
    let edge = harness
        .get_by_label("Resize the end of Windows")
        .rect()
        .center();
    let shift = points(&harness, 10 * GIB);
    drag(&mut harness, edge, edge + Vec2::new(shift, 0.0));
    let after = extent(&harness, 4).start;
    assert!(
        after > before,
        "the recovery partition was pushed right, not blocked"
    );
    // Hover the tiny EFI sliver so it grows to a readable size.
    let efi = harness
        .get_by_label("EFI system partition (after)")
        .rect()
        .center();
    harness.hover_at(efi);
    harness.run();
    snapshot(&mut harness, "20-editor-push-magnify");
}

#[test]
fn cleaning_up_removes_a_partition() {
    let mut harness = home_harness(1440.0);
    harness.get_by_label("Clean up a drive").click();
    harness.run_steps(4);
    assert_eq!(harness.state().goal, Some(Goal::CleanUp));

    // Click the Linux row to mark it for removal.
    harness.get_by_label("partition 5 \"Linux\"").click();
    harness.run_steps(4);
    assert!(
        harness.state().is_deleted(guid(5)),
        "Linux is marked for removal"
    );

    seconds(&mut harness, 0.4);
    let plan = harness
        .state()
        .plan
        .clone()
        .expect("a plan after the change");
    assert!(
        plan.operations
            .iter()
            .any(|op| matches!(op, bootshift_core::plan::Operation::Delete { .. })),
        "the plan deletes the partition"
    );
    snapshot(&mut harness, "30-cleanup");
}

#[test]
fn give_space_grows_by_shrinking_a_neighbour() {
    let mut harness = home_harness(1440.0);
    harness.get_by_label("Give a partition more space").click();
    harness.run_steps(4);
    assert_eq!(harness.state().screen, Screen::Arrange);
    assert_eq!(harness.state().goal, Some(Goal::GiveSpace));

    let app = harness.state();
    let grown_guid = app.goal_partition.expect("a default partition to grow");
    let probed = app.probed.as_ref().expect("a disk");
    let cap = crate::editing::grow_capacity(&probed.layout, &probed.target, grown_guid).0;
    assert!(cap > 0, "there is room to grow by shrinking a neighbour");
    let sector = probed.layout.geometry.sector_size;
    let max_label = format!(
        "Max · {}",
        bootshift_core::units::format_bytes(cap * sector as u64)
    );
    snapshot(&mut harness, "10-goal-give");

    harness.get_by_label(&max_label).click();
    harness.run_steps(4);
    let app = harness.state();
    let before = demo_layout()
        .partition(grown_guid)
        .expect("the partition")
        .extent
        .len
        .0;
    assert!(
        app.target[&grown_guid].len.0 > before,
        "the chosen partition grew"
    );
    let paid = demo_layout().partitions.iter().any(|p| {
        app.target
            .get(&p.unique_guid)
            .is_some_and(|e| e.len.0 < p.extent.len.0)
    });
    assert!(paid, "a neighbour gave up the space");
    snapshot(&mut harness, "11-goal-give-grown");
}

#[test]
fn the_whole_demo_flow_runs_from_drag_to_done() {
    let mut harness = harness(1440.0);
    snapshot(&mut harness, "01-arrange");
    assert!(
        !harness.state().has_changes(),
        "nothing to plan until something moves"
    );

    arrange_the_demo_story(&mut harness);
    seconds(&mut harness, 0.4);
    let app = harness.state();
    let plan = app
        .plan
        .as_ref()
        .expect("the plan arrives 300 ms after the last change");
    assert!(matches!(plan.operations[0], Operation::Shrink { .. }));
    assert_eq!(plan.operations.len(), 3);
    assert_eq!(plan.copy_bytes, (500 << 20) + (90u64 << 30));
    harness.get_by_label("Partition Windows").click();
    harness.run();
    assert_eq!(harness.state().selected, Some(guid(3)));
    snapshot(&mut harness, "02-arranged");

    harness.get_by_label("See the plan").click();
    seconds(&mut harness, 0.3);
    assert_eq!(harness.state().screen, Screen::Plan);
    let app = harness.state();
    let probed = app.probed.as_ref().expect("a disk");
    let steps: Vec<String> = app
        .plan
        .iter()
        .flat_map(|p| &p.operations)
        .map(|op| op.describe(&probed.layout))
        .collect();
    assert_eq!(steps[0], "Shrink partition 3 \"Windows\" to 116.5 GiB");
    assert!(steps[1].starts_with("Move partition 4 \"Windows recovery\" to 116.8 GiB"));
    snapshot(&mut harness, "03-plan");

    harness.get_by_label("Run the safety checks").click();
    seconds(&mut harness, 0.5);
    assert!(
        !harness.state().checks_finished,
        "the rows resolve one at a time"
    );
    seconds(&mut harness, 1.2);
    let app = harness.state();
    assert!(app.checks_passed);
    assert_eq!(
        app.status,
        "All green. Boringly green. Confirm your backup to unlock apply."
    );
    assert!(!app.can_apply(), "apply waits for the backup box");

    harness
        .get_by_label("My important files are backed up somewhere off this disk")
        .click();
    harness.step();
    assert!(harness.state().can_apply());
    snapshot(&mut harness, "04-checks");

    // Apply prepares the change for the restart; the move itself runs in the recovery system at
    // boot, on the machine's own screen, so the app shows an instruction, not a simulation.
    harness.get_by_label("Apply and restart later").click();
    seconds(&mut harness, 0.3);
    let app = harness.state();
    assert!(
        app.prepared_to_restart,
        "apply prepares the change for the restart"
    );
    assert_eq!(app.screen, Screen::Checks);
    assert_eq!(
        app.status,
        "Changes prepared. Restart your computer to apply them."
    );
    assert!(
        !app.can_apply(),
        "there is nothing left to apply once it is prepared"
    );
    snapshot(&mut harness, "05-prepared");
}

/// After the machine restarts and the move finishes, the helper reports the outcome and the app
/// reopens straight onto the finished screen instead of the goal launcher.
#[test]
fn reopens_to_the_finished_screen_after_a_completed_run() {
    let mut harness = Harness::builder()
        .with_size(Vec2::new(1440.0, 900.0))
        .with_step_dt(STEP)
        .with_max_steps(2_000)
        .build_ui_state(
            |ui, app: &mut App| app.frame(ui),
            App::new(Worker::inline(Box::new(DemoBackend::finished()))),
        );
    harness.run_steps(6);
    let app = harness.state();
    assert_eq!(app.screen, Screen::Done, "a finished run reopens onto Done");
    let outcome = app.outcome.as_ref().expect("the finished run's numbers");
    assert_eq!(outcome.chunks, 1448);
    assert_eq!(outcome.power_cuts, 1);
    snapshot(&mut harness, "08-done");

    // From the finished screen, planning another change returns to the editor on the fresh disk.
    harness.get_by_label("Plan another change").click();
    seconds(&mut harness, 0.5);
    let app = harness.state();
    assert_eq!(app.screen, Screen::Arrange);
    assert_eq!(app.target, app.probed.as_ref().expect("a disk").target);
}

#[test]
fn the_arrow_keys_move_and_shift_resizes_by_one_gib() {
    let mut harness = harness(1440.0);
    let swap = demo_layout().partitions[5].extent;
    harness
        .get_by_label("swap: drag to move, drag edges to resize")
        .focus();
    harness.run();
    assert!(
        harness
            .get_by_label("swap: drag to move, drag edges to resize")
            .is_focused()
    );
    harness.key_press(Key::ArrowRight);
    harness.step();
    assert_eq!(
        extent(&harness, 6),
        Extent::new(swap.start + Sectors(GIB), swap.len)
    );
    harness.key_press_modifiers(Modifiers::SHIFT, Key::ArrowLeft);
    harness.step();
    assert_eq!(
        extent(&harness, 6),
        Extent::new(swap.start + Sectors(GIB), Sectors(swap.len.0 - GIB))
    );
}

#[test]
fn a_drag_that_cannot_move_changes_nothing() {
    let mut harness = harness(1440.0);
    let before = harness.state().target.clone();
    let linux = harness
        .get_by_label("Linux: drag to move, drag edges to resize")
        .rect()
        .center();
    drag(&mut harness, linux, linux + Vec2::new(200.0, 0.0));
    assert_eq!(
        harness.state().target,
        before,
        "Linux is boxed in by its neighbours"
    );
}

#[test]
fn the_locked_efi_partition_cannot_be_dragged() {
    let mut harness = harness(1440.0);
    let before = harness.state().target.clone();
    assert!(
        harness
            .query_by_label("EFI system partition: drag to move, drag edges to resize")
            .is_none()
    );
    let efi = harness
        .get_by_label("EFI system partition (after)")
        .rect()
        .center();
    drag(&mut harness, efi, efi + Vec2::new(300.0, 0.0));
    assert_eq!(harness.state().target, before);
}

#[test]
fn a_sliver_is_reached_through_its_capsule() {
    let mut harness = harness(1440.0);
    harness
        .get_by_label("Windows recovery, NTFS, 800.0 MiB")
        .click();
    harness.step();
    assert_eq!(harness.state().selected, Some(guid(4)));
    let block = harness.get_by_label("Windows recovery: drag to move, drag edges to resize");
    assert!(block.is_focused(), "the capsule hands focus to its sliver");
}

#[test]
fn picking_a_table_row_selects_it_and_picking_again_clears_it() {
    let mut harness = harness(1440.0);
    harness.get_by_label("Partition Linux").click();
    harness.step();
    assert_eq!(harness.state().selected, Some(guid(5)));
    harness.get_by_label("Partition Linux").click();
    harness.step();
    assert_eq!(harness.state().selected, None);
}

#[test]
fn steps_ahead_cannot_be_skipped_to() {
    let mut harness = harness(1440.0);
    harness.get_by_label("Checks").click();
    harness.step();
    assert_eq!(harness.state().screen, Screen::Arrange);
}

#[test]
fn every_width_lays_out_without_overflowing() {
    for width in [1600.0, 1180.0, 1100.0, 780.0] {
        let mut harness = harness(width);
        arrange_the_demo_story(&mut harness);
        seconds(&mut harness, 0.4);
        let widest = harness
            .root()
            .children_recursive()
            .filter_map(|node| node.accesskit_node().bounding_box())
            .map(|rect| rect.x1 as f32)
            .fold(0.0, f32::max);
        assert!(widest <= width + 0.5, "{width}: a widget reaches {widest}");
        snapshot(&mut harness, &format!("width-{width}"));
    }
}

#[test]
fn the_migration_goal_is_coming_soon_and_does_not_open() {
    let mut harness = home_harness(1440.0);
    assert_eq!(harness.state().screen, Screen::Home);
    // Migration is not released yet: the card is a teaser and must not open a screen.
    harness.get_by_label("Migrate to new hardware").click();
    harness.run_steps(4);
    assert_eq!(harness.state().screen, Screen::Home);
}

#[test]
fn tab_reaches_the_partition_blocks_in_disk_order() {
    let mut harness = harness(1440.0);
    let order = [
        "Microsoft reserved: drag to move, drag edges to resize",
        "Windows: drag to move, drag edges to resize",
        "Windows recovery: drag to move, drag edges to resize",
    ];
    harness.get_by_label(order[0]).focus();
    harness.step();
    for label in &order[1..] {
        harness.key_press(Key::Tab);
        harness.step();
        assert!(harness.get_by_label(label).is_focused(), "{label}");
    }
}
