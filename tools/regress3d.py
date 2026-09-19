#!/usr/bin/env python3
"""Automated regression checks for the --3d diorama renderer.

Every mechanical property of the diorama is checked here as a number, so the
fix-one-break-another loop stops needing screenshots. The emulator emits a
machine-readable trace under GBA_3D_TRACE=1: one FRAME marker per rendered
frame, a TRACE line with the integrated camera, a FIG line per drawn character
with its anchor in MAP pixels, a SHA of the finished frame, and (under
GBA_3D_GEOM=1) one GEOM line per map cell naming the geometry built on it.

Checks:
  smooth     the camera advances at most one pixel per frame and never
             reverses inside a walking step, and the player's anchor keeps a
             CONSTANT offset from the camera for the whole walk.
  bump       holding a direction into an obstacle never moves the player's
             anchor and never lets it cross into the blocked row.
  anchor     the anchor cell agrees with the game's own player coordinate
             (gSaveBlock1Ptr) on every frame, within one cell of step
             interpolation.
  determin   the same scenario run twice produces identical frame hashes.
  geometry   the same map cells seen from two camera positions produce
             identical geometry.
  interior   walking into Oak's lab and back out again: the player's rendered
             anchor cell equals the game's own coordinate on every frame,
             including the two warp transitions, no drawn figure is anchored
             outside the map or over a cell the diorama leaves empty, and the
             frame hashes are reproducible.
  fallback   a full-screen menu is not the overworld, so it must render as
             flat 2D (no map grid at all) even though the map pointers in RAM
             still read perfectly well.

Usage:  python3 tools/regress3d.py [--rom /tmp/rom.gba] [--only NAME]
Exit code is 0 only when every check passes.
"""
import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile

# Input prefix that loads Joseph's save and dismisses the recap, landing in
# live play in Pallet Town around frame 1500.
PREFIX = ("420-424:start,480-484:a,540-544:a,600-604:a,660-664:a,930-934:a,"
          "1230-1234:a,1290-1294:a,1350-1354:a,1410-1414:a")
LIVE = 1500

# Walk from where the save drops the player round to the front of Oak's lab,
# in through the door (warp at about frame 2570), stand inside, then walk back
# out through the same door (warp back at about frame 2717) and away from it.
# Interiors are where every assumption that the camera is locked to the player
# and that the live grid's border margin is scenery falls apart, so this is the
# scenario that has to hold.
ENTER = ("1520-1555:left,1570-1680:down,1700-1800:left,1810-1930:down,"
         "1950-2060:right,2080-2160:up,2180-2230:down,2250-2330:right,"
         "2350-2420:up,2440-2458:left,2480-2560:up")
EXIT = "2700-2760:down,2800-2860:up,2900-2960:left"
INSIDE = (2600, 2700)
# Frames spent in a warp fade, where the PPU still shows sprites belonging to
# the map that is being left. Anchors are checked there too, but "is there
# floor under this figure" only makes sense once a map has settled.
SETTLE = 24

# Walks the player right up against a stationary Pallet Town NPC and stands
# there, so a second character's sprite sits one cell (sixteen screen pixels)
# from his own. That is the arrangement that used to make the NPC count as the
# player, get pinned to the camera instead of to the map, and slide along with
# every step: the "NPCs follow me like a mirage" report.
NPC = ("1520-1555:left,1570-1680:down,1700-1800:left,1810-1930:down,"
       "1950-2060:left,2080-2200:down")

# Walks to the north-west corner of Pallet Town, where Joseph plays: standing
# left of his house with the west tree border down the side of the frame and the
# town's north tree row behind the fence. Frame 2020 is that view.
NW_PALLET = "1520-1600:down,1620-1900:left,1920-2000:up"
NW_FRAME = 2020

# Opens the START menu and walks into the trainer card, a full-screen menu
# that replaces the overworld while gBackupMapLayout still points at a
# perfectly valid Pallet Town.
MENU = "1520-1524:start,1580-1584:down,1640-1644:a,1760-1764:a"
MENU_OPEN = (1790, 1900)

# From the north-west corner, up onto the row behind his house and then east
# and west along it, so the lifted roof stands between him and the camera for
# most of a hundred frames: the "sinking behind the building" walk.
BEHIND = "2020-2040:up,2060-2260:right,2280-2300:down,2320-2560:left"


class Frame:
    __slots__ = ("n", "cam", "fine", "player", "figs", "sha", "geom",
                 "mapsize", "seen", "hide", "trees", "models")

    def __init__(self, n):
        self.n = n
        self.cam = self.fine = self.player = self.sha = self.mapsize = None
        self.figs = []
        self.seen = set()
        self.hide = []
        self.trees = {}
        self.geom = {}
        self.models = {}


def run(binary, rom, script, frames, dump_from=LIVE, geom=False, extra=None):
    env = dict(os.environ)
    env["GBA_3D_TRACE"] = "1"
    env["GBA_INPUT"] = PREFIX + ("," + script if script else "")
    env["GBA_FRAMES"] = str(frames)
    env["GBA_DUMP_FROM"] = str(dump_from)
    if geom:
        env["GBA_3D_GEOM"] = "1"
    if extra:
        env.update(extra)
    # Headless exit rewrites <rom>.sav, so every run works on a private copy.
    with tempfile.TemporaryDirectory() as td:
        r = os.path.join(td, "rom.gba")
        shutil.copyfile(rom, r)
        if os.path.exists(rom + ".sav"):
            shutil.copyfile(rom + ".sav", r + ".sav")
        out = subprocess.run([binary, r, "--headless", "--3d"], env=env,
                             stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, text=True, check=True)
    return parse(out.stdout)


def parse(text):
    frames, cur = [], None
    for line in text.splitlines():
        f = line.split()
        if not f:
            continue
        if f[0] == "FRAME":
            cur = Frame(int(f[1]))
            frames.append(cur)
        elif cur is None:
            continue
        elif f[0] == "TRACE":
            cur.cam = (int(f[2]), int(f[3]))
            cur.fine = (int(f[5]), int(f[6]))
            cur.player = (int(f[12]), int(f[13]))
            if len(f) >= 17:
                cur.mapsize = (int(f[15]), int(f[16]))
        elif f[0] == "FIG":
            m = re.match(
                r"FIG (\d+) box (\S+) (\S+) (\S+) mappix (-?\d+) (-?\d+) "
                r"anchor (-?\d+) (-?\d+) cell (-?\d+) (-?\d+) ground (\S+) "
                r"wz (\S+) gamecell (-?\d+) (-?\d+) player (\d) void (\d)",
                line)
            if m and line.strip() not in cur.seen:
                # The headless runner renders its final frame once more on the
                # way out, so the last FRAME block carries every figure twice.
                cur.seen.add(line.strip())
                g = m.groups()
                cur.figs.append(dict(
                    idx=int(g[0]), feet=float(g[3]),
                    minx=float(g[1]), maxx=float(g[2]),
                    anchor=(int(g[6]), int(g[7])),
                    cell=(int(g[8]), int(g[9])), ground=float(g[10]),
                    wz=float(g[11]), gamecell=(int(g[12]), int(g[13])),
                    player=g[14] == "1", void=g[15] == "1"))
        elif f[0] == "HIDE":
            cur.hide.append(dict(idx=int(f[1]), player=f[3] == "1",
                                 behind=f[5] == "1", cov=int(f[7]),
                                 vis=int(f[9]), ghost=f[11] == "1"))
        elif f[0] == "TREE":
            cur.trees[(int(f[1]), int(f[2]))] = (int(f[4]), int(f[6]))
        elif f[0] == "MODEL":
            cur.models[(f[1], int(f[2]), int(f[3]))] = tuple(f[4:])
        elif f[0] == "SHA":
            cur.sha = f[1]
        elif f[0] == "GEOM":
            cur.geom[(int(f[1]), int(f[2]))] = f[3]
    return frames


def player_fig(fr):
    for g in fr.figs:
        if g["player"]:
            return g
    return None


# ---------------------------------------------------------------------------

def check_smooth(frames, lo, hi, axis):
    """Camera moves at most 1px/frame, and the player's anchor keeps a fixed
    offset from the camera for the whole walk."""
    bad, offs = [], set()
    win = [f for f in frames if lo <= f.n <= hi and f.cam]
    for a, b in zip(win, win[1:]):
        d = b.cam[axis] - a.cam[axis]
        if abs(d) > 1:
            bad.append(f"frame {b.n}: camera jumped {d}px")
    for f in win:
        g = player_fig(f)
        if g:
            offs.add((g["anchor"][0] - f.cam[0], g["anchor"][1] - f.cam[1]))
    if len(offs) > 1:
        bad.append(f"player anchor offset from camera varied: {sorted(offs)}")
    moved = abs(win[-1].cam[axis] - win[0].cam[axis]) if win else 0
    return bad, f"{len(win)} frames, camera moved {moved}px, {len(offs)} distinct anchor offsets"


def check_bump(frames, lo, hi):
    """During a bump the player's anchor must not move at all."""
    bad = []
    win = [f for f in frames if lo <= f.n <= hi and player_fig(f)]
    anchors = {player_fig(f)["anchor"] for f in win}
    cams = {f.cam for f in win}
    if len(cams) != 1:
        return [f"not a bump: camera moved over {len(cams)} positions"], "n/a"
    if len(anchors) != 1:
        bad.append(f"player anchor moved during bump: {sorted(anchors)}")
    return bad, f"{len(win)} frames, {len(anchors)} distinct anchors"


def check_anchor(frames, lo, hi):
    """Anchor cell must agree with the game's own player coordinate."""
    bad, worst = [], 0
    for f in frames:
        if not (lo <= f.n <= hi):
            continue
        g = player_fig(f)
        if not g:
            continue
        dx = g["cell"][0] - g["gamecell"][0]
        dy = g["cell"][1] - g["gamecell"][1]
        worst = max(worst, abs(dx), abs(dy))
        if abs(dx) > 1 or abs(dy) > 1:
            bad.append(f"frame {f.n}: anchor cell {g['cell']} vs game {g['gamecell']}")
    return bad[:5], f"worst disagreement {worst} cell(s)"


def check_ground(frames, lo, hi):
    """A walking character never stands on water."""
    bad = []
    for f in frames:
        if not (lo <= f.n <= hi):
            continue
        g = player_fig(f)
        if g and g["ground"] < 0:
            bad.append(f"frame {f.n}: player standing on water (ground {g['ground']})")
    return bad[:5], f"{len(bad)} frames on water"


def settled(frames):
    """Frames whose map has been the same one for SETTLE frames: everything
    outside those windows is a warp fade, where the PPU is still drawing
    sprites that belong to the map being left."""
    out, since = [], None
    last = object()
    for f in frames:
        if f.mapsize != last:
            last, since = f.mapsize, f.n
        if f.mapsize and f.n - since >= SETTLE:
            out.append(f)
    return out


def check_figures(frames):
    """No drawn figure stands outside the map, and once a map has settled none
    of them stands over a cell the diorama leaves empty. Both were the same
    bug: the player and the NPCs beside him drawn one or two rows off the
    floor, hanging in the black void south and east of an interior."""
    bad, off, floating = [], 0, 0
    for f in frames:
        if not f.mapsize:
            continue
        w, h = f.mapsize
        for g in f.figs:
            cx, cy = g["cell"]
            if not (0 <= cx < w and 0 <= cy < h):
                off += 1
                if len(bad) < 5:
                    bad.append(f"frame {f.n}: figure {g['idx']} at cell "
                               f"{g['cell']}, map is {w}x{h}")
    for f in settled(frames):
        for g in f.figs:
            if g["void"]:
                floating += 1
                if len(bad) < 5:
                    bad.append(f"frame {f.n}: figure {g['idx']} at cell "
                               f"{g['cell']} has no floor under it")
    n = sum(len(f.figs) for f in frames)
    return bad, f"{n} figures drawn, {off} outside the map, {floating} floating"


def check_warps(frames):
    """A warp must resync the anchor, not carry the old map's offset into the
    new one: within SETTLE frames of every map change the player's anchor cell
    must be the game's own coordinate exactly."""
    bad, warps = [], 0
    last = object()
    since = 0
    for f in frames:
        if f.mapsize != last:
            last, since, warps = f.mapsize, f.n, warps + 1
        g = player_fig(f)
        if not g or f.n - since < SETTLE or not f.mapsize:
            continue
        dx = g["cell"][0] - g["gamecell"][0]
        dy = g["cell"][1] - g["gamecell"][1]
        if abs(dx) > 1 or abs(dy) > 1:
            bad.append(f"frame {f.n} ({f.n - since} after a map change): "
                       f"anchor {g['cell']} vs game {g['gamecell']}")
    return bad[:5], f"{warps - 1} map changes, {len(frames)} frames"


def check_furniture(frames):
    """Items sit on the table, while the player stays on the floor."""
    frames = [f for f in frames if f.mapsize == (13, 14) and f.geom]
    if not frames:
        return ["no lab geometry captured"], "n/a"
    f = frames[-1]
    bad = []
    for cell in [(8, 4), (9, 4), (10, 4), (4, 1), (5, 1)]:
        shape = f.geom.get(cell, "")
        height = float(shape[1:].split(":")[0]) if shape.startswith("K") else 0
        items = [g for g in f.figs if g["cell"] == cell and not g["player"]]
        if height <= 0 or len(items) != 1 or items[0]["ground"] != height:
            bad.append(f"item at {cell} is not seated on its table surface")
        elif cell[1] == 1:
            visibility = next((h for h in f.hide if h["idx"] == items[0]["idx"]), None)
            if not visibility or visibility["vis"] < visibility["cov"] * 0.95:
                bad.append(f"book at {cell} is still clipped by the desk or wall")
    player = player_fig(f)
    if not player or player["ground"] != 0:
        bad.append("player was lifted off the aisle floor")
    if not f.geom.get((1, 8), "").startswith("K"):
        bad.append("bookcase is still a flat billboard")
    return bad, "three balls and two books on surfaces; books visible; player on floor"


def check_aisle_marker(frames):
    bad, checked = [], 0
    for f in frames:
        player = player_fig(f)
        if not player or player["cell"] not in [(7, 5), (8, 5)]:
            continue
        h = next((h for h in f.hide if h["player"]), None)
        if h:
            checked += 1
            if h["ghost"] or h["vis"] != h["cov"]:
                bad.append(f"frame {f.n}: marker or occlusion in the open aisle")
    if checked < 20:
        bad.append(f"only {checked} aisle frames checked")
    return bad[:5], f"{checked} frames beside the table, {len(bad)} false occlusions"


def check_npc(frames):
    """Only one figure is ever the player, and every other figure stands on
    the map rather than travelling with the camera.

    The cause is checked directly: a second figure flagged as the player is
    pinned to the camera by design, so a character in a neighbouring cell used
    to be dragged along by every step the player took, which is the "the NPCs
    follow me, like a mirage" report.

    The symptom is checked by the one invariant a character on a tile map
    cannot break. Characters stand on cells, and a step is a whole cell, so
    wherever a figure comes to REST its anchor must differ from where that
    same figure last rested by a whole number of cells. A figure being dragged
    a pixel at a time by the camera comes to rest half way between two cells,
    and no amount of real walking can do that.
    """
    RESTED = 10
    bad, multi, near, drift, jumps = [], 0, 0, 0, 0
    tracks = {}          # id -> [anchor, still frames, first rest anchor]
    next_id = [0]
    prev = {}
    for f in frames:
        if sum(1 for g in f.figs if g["player"]) > 1:
            multi += 1
            if len(bad) < 6:
                bad.append(f"frame {f.n}: two figures claim to be the player")
        for g in f.figs:
            cx = (g["minx"] + g["maxx"]) / 2
            if not g["player"] and abs(cx - 120) < 24 and abs(g["feet"] - 88) < 24:
                near += 1
        cur = {}
        for g in f.figs:
            if g["player"]:
                continue
            a = g["anchor"]
            best = None
            for t, st in prev.items():
                d = abs(a[0] - st[0][0]) + abs(a[1] - st[0][1])
                if d <= 24 and (best is None or d < best[0]):
                    best = (d, t)
            if best is None:
                t = next_id[0]
                next_id[0] += 1
                tracks[t] = [a, 1, None]
            else:
                t = best[1]
                st = tracks[t]
                if max(abs(a[0] - st[0][0]), abs(a[1] - st[0][1])) > 2:
                    jumps += 1
                    if len(bad) < 6:
                        bad.append(f"frame {f.n}: figure anchor jumped from "
                                   f"{st[0]} to {a}")
                st[1] = st[1] + 1 if a == st[0] else 1
                st[0] = a
                if st[1] == RESTED:
                    if st[2] is None:
                        st[2] = a
                    elif (a[0] - st[2][0]) % 16 or (a[1] - st[2][1]) % 16:
                        drift += 1
                        if len(bad) < 6:
                            bad.append(
                                f"frame {f.n}: figure came to rest at {a}, "
                                f"which is not a whole number of cells from "
                                f"{st[2]} where it last stood")
                        st[2] = a
            cur[t] = tracks[t]
        prev = cur
    if not near:
        bad.append("scenario never stood an NPC beside the player, so it "
                   "proves nothing")
    return bad, (f"{len(frames)} frames, {near} with an NPC beside the player, "
                 f"{len(tracks)} figure tracks, {multi} double-player, "
                 f"{drift} off-grid rests, {jumps} jumps")


def check_sink(frames):
    """The player is never left half drawn behind a building.

    A building is stood up off the ground, which moves its whole picture north
    on screen, so it covers the walkable rows BEHIND it and cuts the character
    walking along them off from the feet upwards, a little more with every
    step -- he reads as walking down a staircase. Wherever the map says a
    built volume stands on the rows between the player and the camera, any
    part of him the volume hides has to come back as the silhouette.
    """
    bad, behind, hidden, missing = [], 0, 0, 0
    for f in frames:
        for h in f.hide:
            if not h["player"] or not h["behind"]:
                continue
            behind += 1
            if h["cov"] == h["vis"]:
                continue
            hidden += 1
            if not h["ghost"]:
                missing += 1
                if len(bad) < 5:
                    frac = (h["cov"] - h["vis"]) / max(h["cov"], 1)
                    bad.append(f"frame {f.n}: player {frac:.0%} hidden by the "
                               "volume in front of him, no silhouette drawn")
    if not hidden:
        bad.append("the player never walked behind a built volume, so this "
                   "scenario proves nothing")
    return bad, (f"{behind} frames behind a volume, {hidden} of them with the "
                 f"player partly hidden, {missing} without a silhouette")


def check_trees(frames, at):
    """Every pixel a tree billboard paints is still exactly that colour in the
    finished frame.

    The renderer records the color each tree paints, from lit mesh materials
    in modeled mode or the original artwork in sprite mode. Post-processing
    must preserve those colors in the central focus area. This is a stability
    check, not a claim that modeled trees match the 2D artwork pixel for pixel.
    """
    win = [f for f in frames if f.n == at and f.trees]
    if not win:
        return [f"frame {at} drew no trees, so this proves nothing"], "n/a"
    fr = win[-1]
    drawn = {c: v for c, v in fr.trees.items() if v[0] > 0}
    bad = [f"tree at map cell {c}: {v[1]} of {v[0]} pixels changed after "
           "they were painted" for c, v in sorted(drawn.items()) if v[1]]
    if len(drawn) < 8:
        bad.append(f"only {len(drawn)} trees on screen at frame {at}")
    painted = sum(v[0] for v in drawn.values())
    return bad[:5], (f"{len(drawn)} trees, {painted} pixels, "
                     f"{sum(v[1] for v in drawn.values())} altered")


def check_flat(frames, lo, hi, want):
    """`want` True: these frames are the overworld and must build a diorama.
    False: they are not, and must fall through to flat 2D, which shows up as
    no map grid being accepted for the frame at all."""
    win = [f for f in frames if lo <= f.n <= hi]
    got = [f for f in win if f.cam]
    if not win:
        return ["no frames in the window"], "n/a"
    if want and len(got) < len(win):
        return [f"{len(win) - len(got)} of {len(win)} frames had no map grid"], \
            "diorama expected"
    if not want and got:
        return [f"{len(got)} of {len(win)} frames still built a diorama, "
                f"first at frame {got[0].n}"], "flat 2D expected"
    return [], f"{len(win)} frames, {len(got)} with a diorama"


def check_determinism(a, b):
    sa = [(f.n, f.sha) for f in a]
    sb = [(f.n, f.sha) for f in b]
    if sa != sb:
        first = next((x for x, y in zip(sa, sb) if x != y), None)
        return [f"frame hashes diverge, first at {first}"], "differs"
    return [], f"{len(sa)} frames identical"


def check_geometry(a, b):
    """Same map cells, two camera positions, identical geometry."""
    ga, gb = a[-1].geom, b[-1].geom
    common = set(ga) & set(gb)
    diff = [c for c in common if ga[c] != gb[c]]
    if not common:
        return ["no overlapping cells between the two views"], "n/a"
    bad = [f"cell {c}: {ga[c]} vs {gb[c]}" for c in sorted(diff)[:6]]
    return bad, f"{len(common)} shared cells, {len(diff)} differ"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rom", default="/tmp/rom.gba")
    ap.add_argument("--binary", default="./target/release/gba")
    ap.add_argument("--only", default=None)
    args = ap.parse_args()

    results = []

    def report(name, bad, note):
        results.append((name, not bad, note, bad))
        print(f"{'PASS' if not bad else 'FAIL'}  {name:<24} {note}")
        for b in bad:
            print(f"        {b}")

    want = lambda n: args.only is None or args.only in n

    if want("walk") or want("bump") or want("anchor"):
        # Hold Down for 100 frames: one free step south, then a bump against
        # the obstacle below for the rest.
        fr = run(args.binary, args.rom, "1520-1620:down", 1620)
        if want("walk"):
            report("walk.smooth", *check_smooth(fr, 1529, 1546, 1))
            report("walk.anchor", *check_anchor(fr, 1500, 1620))
            report("walk.ground", *check_ground(fr, 1500, 1620))
        if want("bump"):
            report("bump.stable", *check_bump(fr, 1560, 1615))

    if want("npc"):
        fr = run(args.binary, args.rom, NPC, 2210)
        report("npc.pinned", *check_npc(fr))
        report("npc.sink", *check_sink(fr))

    if want("interior"):
        # One run all the way in and back out: two warps, an interior with
        # NPCs standing near its south and east edges, and the door animation
        # in between.
        fr = run(args.binary, args.rom, ENTER + "," + EXIT, 3000,
                 dump_from=2400)
        # A warp fade is a black screen with the outgoing map's sprites still
        # on it, so the anchor is only meaningful once a map has settled;
        # interior.warp below is what pins down the transition itself.
        report("interior.anchor", *check_anchor(settled(fr), 2400, 3000))
        report("interior.figures", *check_figures(fr))
        report("interior.warp", *check_warps(fr))
        report("interior.inside",
               *check_flat(fr, INSIDE[0], INSIDE[1], True))
        b = run(args.binary, args.rom, ENTER + "," + EXIT, 3000,
                dump_from=2400)
        report("interior.determinism", *check_determinism(fr, b))

    if want("interior") or want("furniture"):
        furniture = run(args.binary, args.rom, ENTER + ",2680-2790:up", 2820,
                        dump_from=2818, geom=True)
        report("interior.furniture", *check_furniture(furniture))

    if want("interior") or want("occlusion"):
        aisle = run(args.binary, args.rom, ENTER + ",2680-2790:up,2820-2855:right",
                    2890, dump_from=2818)
        report("interior.occlusion", *check_aisle_marker(aisle))

    if want("trees"):
        # His own viewpoint, not a convenient one: the west tree border and the
        # town's north tree row, seen from where he stands in the screenshot.
        # Pinned to GBA_3D_WALL=1, which is how he plays.
        wall = {"GBA_3D_WALL": "1"}
        fr = run(args.binary, args.rom, NW_PALLET, NW_FRAME,
                 dump_from=NW_FRAME - 2, extra=wall)
        report("trees.pixelmatch", *check_trees(fr, NW_FRAME))
        # And the walk east along the row behind his house, where the roof
        # covers him completely: he must never be left half drawn.
        report("trees.sink", *check_sink(fr + run(
            args.binary, args.rom, NW_PALLET + "," + BEHIND, 2560,
            dump_from=2300, extra=wall)))

    if want("fallback"):
        # A full-screen menu leaves gBackupMapLayout and gSaveBlock1Ptr
        # pointing at a perfectly good Pallet Town, so pointer validity cannot
        # tell it from the overworld; only agreement with the pixels the PPU
        # actually drew can.
        fr = run(args.binary, args.rom, MENU, 1900, dump_from=LIVE)
        report("fallback.overworld", *check_flat(fr, 1500, 1515, True))
        report("fallback.menu",
               *check_flat(fr, MENU_OPEN[0], MENU_OPEN[1], False))

    if want("determin"):
        a = run(args.binary, args.rom, "1520-1560:down", 1560)
        b = run(args.binary, args.rom, "1520-1560:down", 1560)
        report("determinism", *check_determinism(a, b))

    if want("geometry"):
        # The same neighbourhood seen from two camera positions: standing
        # still, and after walking two cells west.
        a = run(args.binary, args.rom, "", 1540, dump_from=1539, geom=True)
        b = run(args.binary, args.rom, "1505-1560:left", 1600, dump_from=1599,
                geom=True)
        report("geometry.stable", *check_geometry(a, b))

    if want("models") and os.environ.get("GBA_3D_STYLE") == "modeled":
        a = run(args.binary, args.rom, "", 1540, dump_from=1539)
        b = run(args.binary, args.rom, "1505-1560:left", 1600, dump_from=1599)
        first, second = a[-1].models, b[-1].models
        common = set(first) & set(second)
        bad = [f"{key}: model changed while walking" for key in common
               if first[key] != second[key]]
        for kind in ("tree", "building"):
            if not any(key[0] == kind for key in common):
                bad.append(f"no shared {kind} models, so this proves nothing")
        report("models.stable", bad, f"{len(common)} shared model instances")
        inside = run(args.binary, args.rom, ENTER, 2650, dump_from=2649)
        report("models.interior", ["outdoor models appeared inside Oak's lab"]
               if any(f.models for f in inside) else [],
               "indoor furniture retains its own geometry")

    print()
    fails = [r for r in results if not r[1]]
    print(f"{len(results) - len(fails)}/{len(results)} checks passed")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
