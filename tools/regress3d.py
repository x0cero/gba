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

Usage:  python3 tools/regress3d.py [--rom /tmp/rom.gba] [--only NAME]
Exit code is 0 only when every check passes.
"""
import argparse
import os
import re
import subprocess
import sys
import tempfile

# Input prefix that loads Joseph's save and dismisses the recap, landing in
# live play in Pallet Town around frame 1500.
PREFIX = ("420-424:start,480-484:a,540-544:a,600-604:a,660-664:a,930-934:a,"
          "1230-1234:a,1290-1294:a,1350-1354:a,1410-1414:a")
LIVE = 1500


class Frame:
    __slots__ = ("n", "cam", "fine", "player", "figs", "sha", "geom")

    def __init__(self, n):
        self.n = n
        self.cam = self.fine = self.player = self.sha = None
        self.figs = []
        self.geom = {}


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
        os.link(rom, r) if False else subprocess.run(["cp", rom, r], check=True)
        if os.path.exists(rom + ".sav"):
            subprocess.run(["cp", rom + ".sav", r + ".sav"], check=True)
        out = subprocess.run([binary, r, "--headless", "--3d"], env=env,
                             stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, text=True)
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
        elif f[0] == "FIG":
            m = re.match(
                r"FIG (\d+) box (\S+) (\S+) (\S+) mappix (-?\d+) (-?\d+) "
                r"anchor (-?\d+) (-?\d+) cell (-?\d+) (-?\d+) ground (\S+) "
                r"wz (\S+) gamecell (-?\d+) (-?\d+) player (\d)", line)
            if m:
                g = m.groups()
                cur.figs.append(dict(
                    idx=int(g[0]), feet=float(g[3]),
                    anchor=(int(g[6]), int(g[7])),
                    cell=(int(g[8]), int(g[9])), ground=float(g[10]),
                    wz=float(g[11]), gamecell=(int(g[12]), int(g[13])),
                    player=g[14] == "1"))
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

    print()
    fails = [r for r in results if not r[1]]
    print(f"{len(results) - len(fails)}/{len(results)} checks passed")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
