//! Indoor furniture uses complete artwork units, not independent tile slices.
use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Part {
    #[default]
    Ordinary,
    Wall(u8),
    Shelf(u8),
    Table(u8),
    Desk(u8),
    Display,
    Machine(u8, u8),
}

pub(super) fn classify(
    x: i32,
    y: i32,
    back_wall: bool,
    lab: bool,
    behavior: &impl Fn(i32, i32) -> u32,
) -> Part {
    if lab && (4..=5).contains(&x) && (0..=2).contains(&y) {
        return Part::Desk(y as u8);
    }
    if back_wall && (0..2).contains(&y) {
        return Part::Wall(y as u8);
    }
    // MB_BOOKSHELF is the collision/interact row. Its cap and plinth are
    // drawn in the adjacent walkable rows, but belong to the same cabinet.
    for row in 0..3 {
        let middle = y + 1 - row;
        if middle > 2 && behavior(x, middle) == 0x81 {
            return Part::Shelf(row as u8);
        }
    }
    // Oak's lab has a three-cell table with a separate apron artwork row.
    // The caller verifies the map's tile signature before enabling this.
    if lab && (8..=10).contains(&x) && (4..=5).contains(&y) {
        return Part::Table((y - 4) as u8);
    }
    if lab && x == 0 && (3..=4).contains(&y) {
        return Part::Display;
    }
    if lab && (1..=2).contains(&x) && (3..=5).contains(&y) {
        return Part::Machine((x - 1) as u8, (y - 3) as u8);
    }
    Part::Ordinary
}

impl MapGrid {
    pub(super) fn interior_at(&self, x: i32, y: i32) -> Part {
        if !(0..Self::COLS as i32).contains(&x) || !(0..Self::ROWS as i32).contains(&y) {
            Part::Ordinary
        } else {
            self.interior[y as usize * Self::COLS + x as usize]
        }
    }
}

impl Renderer {
    pub(super) fn render_interior_cell(&mut self, g: &MapGrid, cx: i32, cy: i32) -> bool {
        let part = g.interior_at(cx, cy);
        if part == Part::Ordinary {
            return false;
        }
        let slot = g.slot_at(cx, cy, 0);
        let x0 = g.ox + cx as f32 * STEP;
        let x1 = x0 + STEP;
        let zn = g.oz - cy as f32 * STEP;
        let zs = zn - STEP;
        let quad = |north: f32, south: f32, h: f32| {
            [
                project(x0, h, north),
                project(x1, h, north),
                project(x1, h, south),
                project(x0, h, south),
            ]
        };
        if !matches!(part, Part::Wall(_) | Part::Desk(0)) {
            // Remove the cap/base from the floor when it moves onto furniture.
            self.quad_uv(quad(zn, zs, 0.0), &mut |u, v| {
                g.art.bot_at(
                    slot,
                    (u * 16.0).clamp(0.0, 15.0) as usize,
                    (v * 16.0).clamp(0.0, 15.0) as usize,
                )
            });
        }
        match part {
            Part::Desk(1) => {
                // The desk occupies the blocked row in front of the wall.
                // Moving that wall back must preserve its projected upper edge.
                let wall_h = 32.0 - STEP * SIN_P / COS_P;
                let wall_slot = g.slot_at(cx, cy - 1, slot);
                let back = [
                    project(x0, wall_h, zn),
                    project(x1, wall_h, zn),
                    project(x1, 0.0, zn),
                    project(x0, 0.0, zn),
                ];
                self.quad_uv(back, &mut |u, v| {
                    shade(
                        g.art.comp_at(
                            wall_slot,
                            (u * 16.0).clamp(0.0, 15.0) as usize,
                            (v * wall_h).clamp(0.0, 15.0) as usize,
                        ),
                        0.92,
                    )
                });
                let color = g.art.bot_at(g.slot_at(-g.gx0, cy - 1, wall_slot), 8, 2);
                // Keep the room's upper trim continuous across this recess.
                self.quad_uv(quad(zs + 4.0, zs, 32.0), &mut |_, _| shade(color, 0.9));
                let trim = [
                    project(x0, 32.0, zs),
                    project(x1, 32.0, zs),
                    project(x1, 24.0, zs),
                    project(x0, 24.0, zs),
                ];
                self.quad_uv(trim, &mut |u, v| {
                    shade(
                        g.art.comp_at(
                            wall_slot,
                            (u * 16.0).clamp(0.0, 15.0) as usize,
                            (v * 8.0).clamp(0.0, 7.0) as usize,
                        ),
                        0.92,
                    )
                });
            }
            Part::Display => {
                self.render_indoor_art(g, cx, cy, (1, 1), (0.0, 12.0));
                return true;
            }
            Part::Machine(0, 0) => {
                self.render_indoor_art(g, cx, cy, (2, 3), (12.0, 28.0));
                return true;
            }
            Part::Machine(_, _) => return true,
            _ => {}
        }
        let (h, depth, front_rows, top_slot) = match part {
            Part::Wall(0) | Part::Shelf(0 | 2) | Part::Table(1) | Part::Desk(0 | 2) => return true,
            Part::Wall(1) => (32.0, 4.0, 32.0, g.slot_at(cx, cy - 1, slot)),
            Part::Shelf(1) => (20.0, 8.0, 20.0, g.slot_at(cx, cy - 1, slot)),
            Part::Table(0) => (8.0, 16.0, 12.0, slot),
            Part::Desk(1) => (8.0, 16.0, 8.0, slot),
            _ => return false,
        };
        let material = if matches!(part, Part::Wall(_)) {
            // Wall-mounted pictures must not change the wall's upper edge.
            g.art.bot_at(g.slot_at(-g.gx0, cy - 1, top_slot), 8, 2)
        } else {
            g.art.flat[top_slot]
        };
        self.quad_uv(quad(zs + depth, zs, h), &mut |u, v| {
            if matches!(part, Part::Wall(_)) {
                return shade(material, 0.9);
            }
            let py = match part {
                // The lid crosses a metatile boundary: cap rows 9..15 and
                // body rows 0..3. None of that white lid belongs on the front.
                Part::Shelf(_) => 9.0 + v.clamp(0.0, 0.999) * 11.0,
                Part::Wall(_) => 2.0,
                Part::Desk(_) => 5.0 + v * 10.0,
                _ => v * 15.0,
            };
            let src = if matches!(part, Part::Shelf(_)) {
                g.slot_at(cx, cy - 1 + py as i32 / 16, top_slot)
            } else {
                top_slot
            };
            let c = g
                .art
                .object_at(src, (u * 16.0).clamp(0.0, 15.0) as usize, py as usize % 16);
            if c == SKIP { material } else { c }
        });
        let front = [
            project(x0, h, zs),
            project(x1, h, zs),
            project(x1, 0.0, zs),
            project(x0, 0.0, zs),
        ];
        self.quad_uv(front, &mut |u, v| {
            let y = (v.clamp(0.0, 0.999) * front_rows) as usize
                + if matches!(part, Part::Shelf(_)) { 4 } else { 0 };
            let start = match part {
                Part::Wall(_) => cy - 1,
                Part::Table(_) | Part::Desk(_) => cy + 1,
                _ => cy,
            };
            let src = g.slot_at(cx, start + y as i32 / 16, slot);
            let px = (u * 16.0).clamp(0.0, 15.0) as usize;
            let c = if matches!(part, Part::Wall(_)) {
                g.art.comp_at(src, px, y % 16)
            } else {
                g.art.object_at(src, px, y % 16)
            };
            if c == SKIP { SKIP } else { shade(c, 0.92) }
        });
        for (nx, x, light) in [(cx - 1, x0, 0.68), (cx + 1, x1, 0.78)] {
            if g.interior_at(nx, cy) == part {
                continue;
            }
            let side = [
                project(x, h, zs + depth),
                project(x, h, zs),
                project(x, 0.0, zs),
                project(x, 0.0, zs + depth),
            ];
            self.quad_uv(side, &mut |_, v| shade(material, light * (1.0 - v * 0.15)));
        }
        true
    }

    /// Equipment art already contains its curved surfaces and perspective.
    /// Keep that silhouette intact, with a shallow dark backing for thickness.
    fn render_indoor_art(
        &mut self,
        g: &MapGrid,
        cx: i32,
        cy: i32,
        size: (i32, i32),
        shape: (f32, f32),
    ) {
        let (w, h) = size;
        let (trim, height) = shape;
        let x0 = g.ox + cx as f32 * STEP;
        let x1 = x0 + w as f32 * STEP;
        let base = g.oz - (cy + h) as f32 * STEP;
        for (depth, light) in [(1.5, 0.65), (0.75, 0.8), (0.0, 1.0)] {
            let q = [
                project(x0, height * COS_P, base + height * SIN_P + depth),
                project(x1, height * COS_P, base + height * SIN_P + depth),
                project(x1, 0.0, base + depth),
                project(x0, 0.0, base + depth),
            ];
            self.quad_uv(q, &mut |u, v| {
                let x = (u.clamp(0.0, 0.999) * w as f32 * 16.0) as usize;
                let y = (trim + v.clamp(0.0, 0.999) * (h as f32 * 16.0 - trim)) as usize;
                let slot = g.slot_at(cx + x as i32 / 16, cy + y as i32 / 16, 0);
                let c = g.art.object_at(slot, x % 16, y % 16);
                if c == SKIP { SKIP } else { shade(c, light) }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bookshelf_includes_its_walkable_cap_and_base() {
        let behavior = |x, y| if x == 3 && y == 8 { 0x81 } else { 0 };
        assert_eq!(classify(3, 7, false, false, &behavior), Part::Shelf(0));
        assert_eq!(classify(3, 8, false, false, &behavior), Part::Shelf(1));
        assert_eq!(classify(3, 9, false, false, &behavior), Part::Shelf(2));
        assert!(classify(3, 10, false, false, &behavior) == Part::Ordinary);
        assert!(classify(4, 8, false, false, &behavior) == Part::Ordinary);
    }
    #[test]
    fn lab_table_requires_verified_profile_and_wall_keeps_back_fixtures() {
        assert!(classify(8, 4, false, false, &|_, _| 0) == Part::Ordinary);
        assert!(classify(8, 4, true, true, &|_, _| 0) == Part::Table(0));
        assert!(classify(8, 5, true, true, &|_, _| 0) == Part::Table(1));
        assert!(classify(8, 1, true, true, &|_, _| 0x81) == Part::Wall(1));
    }

    #[test]
    fn equipment_keeps_its_complete_outline_without_claiming_the_aisle() {
        for y in 3..=5 {
            for x in 1..=2 {
                assert_eq!(
                    classify(x, y, true, true, &|_, _| 0),
                    Part::Machine((x - 1) as u8, (y - 3) as u8)
                );
                assert_eq!(classify(x, y, true, false, &|_, _| 0), Part::Ordinary);
            }
        }
        assert_eq!(classify(0, 3, true, true, &|_, _| 0), Part::Display);
        assert_eq!(classify(0, 4, true, true, &|_, _| 0), Part::Display);
        assert_eq!(classify(1, 6, true, true, &|_, _| 0), Part::Ordinary);
        assert_eq!(classify(3, 4, true, true, &|_, _| 0), Part::Ordinary);
    }
}
