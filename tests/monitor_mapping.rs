//! Scenario (f): multi-monitor mapping.
//!
//! `monitor_index_at` / `virtual_to_local` / `local_to_virtual` round-trips
//! across negative virtual-screen coordinates (monitors left of / above the
//! primary monitor).
//!
//! Layout used (physical px, virtual-screen space):
//! ```text
//!            +--------+
//!            | M2     |   (0, -1080) 1920x1080   above primary (negative y)
//! +--------+-+--------+
//! | M1     | | M0     |
//! |        | |        |   M1 (-1920, 0) 1920x1080  left (negative x)
//! +--------+-+--------+   M0 (0, 0) 1920x1080      primary
//! ```

use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::composite::{local_to_virtual, monitor_index_at, virtual_to_local};

fn monitors() -> [Rect; 3] {
    [
        Rect::new(0, 0, 1920, 1080),     // M0: primary
        Rect::new(-1920, 0, 1920, 1080), // M1: left of primary (negative x)
        Rect::new(0, -1080, 1920, 1080), // M2: above primary (negative y)
    ]
}

#[test]
fn monitor_index_at_resolves_points_including_negative_coords() {
    let mons = monitors();

    // Primary monitor: corners and center (left/top inclusive, right/bottom exclusive).
    assert_eq!(monitor_index_at(Point::new(0, 0), &mons), Some(0));
    assert_eq!(monitor_index_at(Point::new(1919, 1079), &mons), Some(0));
    assert_eq!(monitor_index_at(Point::new(960, 540), &mons), Some(0));

    // Negative-x monitor.
    assert_eq!(
        monitor_index_at(Point::new(-1920, 0), &mons),
        Some(1),
        "left edge inclusive"
    );
    assert_eq!(monitor_index_at(Point::new(-1, 500), &mons), Some(1));
    assert_eq!(monitor_index_at(Point::new(-1920, 1079), &mons), Some(1));

    // Negative-y monitor.
    assert_eq!(monitor_index_at(Point::new(500, -1), &mons), Some(2));
    assert_eq!(
        monitor_index_at(Point::new(100, -1080), &mons),
        Some(2),
        "top edge inclusive"
    );
    assert_eq!(monitor_index_at(Point::new(1919, -1080), &mons), Some(2));

    // Outside every monitor.
    assert_eq!(
        monitor_index_at(Point::new(-1, -1), &mons),
        None,
        "gap: left of M2 and above M1"
    );
    assert_eq!(
        monitor_index_at(Point::new(1920, 0), &mons),
        None,
        "right edge exclusive"
    );
    assert_eq!(
        monitor_index_at(Point::new(0, 1080), &mons),
        None,
        "bottom edge exclusive"
    );
    assert_eq!(monitor_index_at(Point::new(-1921, 0), &mons), None);
    assert_eq!(monitor_index_at(Point::new(500, -1081), &mons), None);
    assert_eq!(monitor_index_at(Point::new(-5000, 5000), &mons), None);
}

#[test]
fn virtual_to_local_subtracts_monitor_origin() {
    let mons = monitors();
    let m1 = mons[1];
    assert_eq!(virtual_to_local(Point::new(-1920, 0), m1), Point::new(0, 0));
    assert_eq!(
        virtual_to_local(Point::new(-5, 10), m1),
        Point::new(1915, 10)
    );
    assert_eq!(
        virtual_to_local(Point::new(-1, 1079), m1),
        Point::new(1919, 1079)
    );

    let m2 = mons[2];
    assert_eq!(
        virtual_to_local(Point::new(100, -1080), m2),
        Point::new(100, 0)
    );
    assert_eq!(virtual_to_local(Point::new(0, -1), m2), Point::new(0, 1079));
}

#[test]
fn local_to_virtual_adds_monitor_origin() {
    let mons = monitors();
    let m1 = mons[1];
    assert_eq!(local_to_virtual(Point::new(0, 0), m1), Point::new(-1920, 0));
    assert_eq!(
        local_to_virtual(Point::new(1919, 1079), m1),
        Point::new(-1, 1079)
    );

    let m2 = mons[2];
    assert_eq!(local_to_virtual(Point::new(0, 0), m2), Point::new(0, -1080));
    assert_eq!(
        local_to_virtual(Point::new(100, 1079), m2),
        Point::new(100, -1)
    );

    let m0 = mons[0];
    assert_eq!(local_to_virtual(Point::new(7, 9), m0), Point::new(7, 9));
}

#[test]
fn round_trips_across_all_monitors_including_negative_coords() {
    let mons = monitors();
    // Virtual-space samples: interior + edge points of every monitor.
    let samples = [
        Point::new(0, 0),
        Point::new(1919, 1079),
        Point::new(640, 480),
        Point::new(-1920, 0),
        Point::new(-1, 1079),
        Point::new(-1000, 300),
        Point::new(-1920, 1079),
        Point::new(0, -1080),
        Point::new(1919, -1),
        Point::new(800, -700),
        Point::new(1919, -1080),
    ];
    for v in samples {
        let idx =
            monitor_index_at(v, &mons).unwrap_or_else(|| panic!("{v:?} must be on a monitor"));
        let m = mons[idx];

        // virtual -> local: lands strictly inside the monitor-local frame.
        let local = virtual_to_local(v, m);
        assert!(
            (0..m.width as i32).contains(&local.x) && (0..m.height as i32).contains(&local.y),
            "{v:?} -> local {local:?} outside {m:?}"
        );

        // local -> virtual: exact round-trip.
        assert_eq!(
            local_to_virtual(local, m),
            v,
            "V->L->V round-trip for {v:?}"
        );

        // and the reverse composition starting from the local point.
        assert_eq!(
            virtual_to_local(local_to_virtual(local, m), m),
            local,
            "L->V->L round-trip for {local:?}"
        );

        // The mapped local point maps back to the SAME monitor.
        assert_eq!(
            monitor_index_at(local_to_virtual(local, m), &mons),
            Some(idx)
        );
    }
}

#[test]
fn exhaustive_round_trip_on_strided_grid() {
    // Denser sweep (every 255 px) across all three monitors, both directions.
    let mons = monitors();
    for m in mons {
        let mut lx = 0i32;
        while lx < m.width as i32 {
            let mut ly = 0i32;
            while ly < m.height as i32 {
                let local = Point::new(lx, ly);
                let v = local_to_virtual(local, m);
                assert_eq!(virtual_to_local(v, m), local, "L->V->L at {local:?}");
                assert_eq!(
                    local_to_virtual(virtual_to_local(v, m), m),
                    v,
                    "V->L->V at {v:?}"
                );
                ly += 255;
            }
            lx += 255;
        }
    }
}
