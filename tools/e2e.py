#!/usr/bin/env python3
"""StreetPad end-to-end tests (run from WSL): real controller actions in the real app.

Uses the test-only startup options --debug-actions (scripted controller events), --debug-sample
(page URL + a JS value logged every 500 ms) and --debug-js-ui (script in the settings page).
The user's saved settings (position, bookmarks, profiles...) are backed up first and restored
at the end, whatever happens. Close StreetPad before running; it is restarted afterwards.

Usage: tools/e2e.py [scenario ...]
"""
import json, math, os, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PS = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
EXE_BUILD = os.path.join(ROOT, "target/x86_64-pc-windows-msvc/release/StreetPad.exe")

SV_DAMRAK = "https://www.google.com/maps/@?api=1&map_action=pano&viewpoint=52.37555,4.89620&heading=20&pitch=0"
MAP_DAMRAK = "https://www.google.com/maps/@52.37555,4.89620,19z"


def ps(cmd):
    r = subprocess.run([PS, "-NoProfile", "-Command", cmd], capture_output=True, text=True, cwd="/mnt/c")
    return r.stdout.replace("\r", "").strip()


WIN_TMP = ps("$env:TEMP")
# The user's installed copy (restarted after the tests): StreetPad.exe on the Windows desktop.
DESKTOP_EXE = ps("[Environment]::GetFolderPath('Desktop')") + r"\StreetPad.exe"
TMP = subprocess.run(["wslpath", WIN_TMP], capture_output=True, text=True).stdout.strip()
EXE = WIN_TMP + r"\StreetPad-e2e.exe"
SAMPLES = os.path.join(TMP, "streetpad-samples.txt")


def reg_get():
    path = os.path.join(TMP, "sp-e2e-reg.json")
    ps(f"[IO.File]::WriteAllText('{WIN_TMP}\\sp-e2e-reg.json', (Get-ItemProperty HKCU:\\Software\\StreetPad).Settings)")
    return open(path, encoding="utf-8-sig").read()


def reg_set(text):
    open(os.path.join(TMP, "sp-e2e-reg.json"), "w", encoding="utf-8").write(text)
    ps(f"Set-ItemProperty HKCU:\\Software\\StreetPad -Name Settings -Value ([IO.File]::ReadAllText('{WIN_TMP}\\sp-e2e-reg.json'))")


def kill_all():
    ps("Get-Process StreetPad,StreetPad-e2e -EA SilentlyContinue | Stop-Process -Force; Start-Sleep 2")


def count_running():
    return int(ps("(Get-Process StreetPad-e2e -EA SilentlyContinue | measure).Count") or 0)


def run(url, actions=(), sample="1", seconds=20, extra=(), keep=False):
    """Launch, play actions (delays from launch+10 s), log samples, then kill. Returns samples."""
    open(os.path.join(TMP, "sp-e2e-actions.json"), "w").write(json.dumps(list(actions)))
    if os.path.exists(SAMPLES):
        os.remove(SAMPLES)
    open(os.path.join(TMP, "sp-e2e-sample.js"), "w").write(sample)
    args = ["--debug-actions", WIN_TMP + r"\sp-e2e-actions.json", "--debug-sample", "@" + WIN_TMP + r"\sp-e2e-sample.js"] + list(extra)
    if url:
        args = ["--url", url] + args
    arglist = ",".join("'" + a.replace("'", "''") + "'" for a in args)
    ps(f"Start-Process '{EXE}' -ArgumentList {arglist}")
    time.sleep(seconds)
    if not keep:
        kill_all()
    out = []
    if os.path.exists(SAMPLES):
        for line in open(SAMPLES, encoding="utf-8", errors="replace"):
            try:
                d = json.loads(line)
                v = d.get("v", "")
                try:
                    v = json.loads(v)
                except Exception:
                    pass
                out.append({"t": d["t"], "url": d["url"], "v": v})
            except Exception:
                pass
    return out


def view(url):
    m = re.search(r"/@(-?[\d.]+),(-?[\d.]+),[^/]*?([\d.]+)y,([\d.]+)h,([\d.]+)t", url)
    if not m:
        return None
    lat, lng, fov, h, t = map(float, m.groups())
    return {"lat": lat, "lng": lng, "fov": fov, "h": h, "t": t}


def pos(url):
    m = re.search(r"/@(-?[\d.]+),(-?[\d.]+),", url)
    return (float(m.group(1)), float(m.group(2))) if m else None


def dist(a, b):
    la1, lo1, la2, lo2 = map(math.radians, (a[0], a[1], b[0], b[1]))
    x = math.sin((la2 - la1) / 2) ** 2 + math.cos(la1) * math.cos(la2) * math.sin((lo2 - lo1) / 2) ** 2
    return 6371000 * 2 * math.atan2(math.sqrt(x), math.sqrt(1 - x))


def wrap(d):
    d = (d + 180) % 360 - 180
    return d


def last_view(samples, before_ms=None, after_ms=None):
    vs = [(s["t"], view(s["url"])) for s in samples if view(s["url"])]
    if before_ms is not None:
        vs = [x for x in vs if x[0] < before_ms]
    if after_ms is not None:
        vs = [x for x in vs if x[0] >= after_ms]
    return vs[-1][1] if vs else None


# ---------------------------------------------------------------- scenarios
# Action delays are from launch + 10 s; sample times are from launch.

def t_turn_around():
    s = run(SV_DAMRAK, [[0, "action", "turn_around"]], seconds=19)
    a, b = last_view(s, before_ms=10000), last_view(s, after_ms=17000)
    assert a and b, "no Street View camera in URL"
    err = abs(wrap(b["h"] - (a["h"] + 180)))
    assert err < 6, f"heading {a['h']:.1f} -> {b['h']:.1f} (error {err:.1f} deg)"
    return f"heading {a['h']:.1f} -> {b['h']:.1f} (error {err:.1f} deg)"


def t_snap_turns():
    s = run(SV_DAMRAK, [[0, "action", "snap_right"], [5000, "action", "snap_left"]], seconds=23)
    a = last_view(s, before_ms=10000)
    m = last_view(s, before_ms=15000, after_ms=13500)
    b = last_view(s, after_ms=21000)
    assert a and m and b, "no Street View camera in URL"
    e1, e2 = abs(wrap(m["h"] - (a["h"] + 90))), abs(wrap(b["h"] - a["h"]))
    assert e1 < 6 and e2 < 6, f"right: error {e1:.1f}, left back: error {e2:.1f}"
    return f"+90 error {e1:.1f} deg, back error {e2:.1f} deg"


def t_level_view():
    url = SV_DAMRAK.replace("pitch=0", "pitch=-30")
    s = run(url, [[0, "zoom", "2"], [2500, "action", "level_view"]], seconds=24)
    a, b = last_view(s, before_ms=10000), last_view(s, after_ms=22000)
    assert a and b, "no Street View camera in URL"
    assert abs(b["t"] - 90) < 3 and abs(b["fov"] - 75) < 9, f"tilt {b['t']:.1f}, fov {b['fov']:.1f}"
    return f"tilt {a['t']:.1f} -> {b['t']:.1f}, fov -> {b['fov']:.1f}"


def t_walk():
    s = run(SV_DAMRAK, [[0, "step", "fwd"], [2500, "step", "fwd"]], seconds=18)
    a, b = last_view(s, before_ms=10000), last_view(s, after_ms=16000)
    assert a and b, "no Street View camera in URL"
    d = dist((a["lat"], a["lng"]), (b["lat"], b["lng"]))
    assert d > 3, f"moved only {d:.1f} m"
    return f"walked {d:.1f} m in 2 steps"


def t_bookmarks():
    before = json.loads(reg_get()).get("bookmarks", [])
    run(SV_DAMRAK, [[0, "action", "bookmark_save"], [2500, "action", "bookmark_save"]], seconds=16)
    after = json.loads(reg_get()).get("bookmarks", [])
    assert len(after) == len(before) + 1, f"bookmarks {len(before)} -> {len(after)} (expected +1, second save = same spot)"
    return f"saved '{after[-1]['name']}', duplicate ignored"


def t_immersive():
    js = "(!!document.getElementById('sp-immersive-style'))+','+(window.__spNoText===true)"
    s = run(SV_DAMRAK, [[0, "action", "immersive"], [8000, "action", "immersive"]], sample=js, seconds=24)
    on = [x["v"] for x in s if 14000 <= x["t"] <= 17500]
    off = [x["v"] for x in s if x["t"] >= 22000]
    assert on and all(v == "true,true" for v in on), f"while on: {on[:3]}"
    assert off and all(v == "false,false" for v in off), f"after off: {off[:3]}"
    return "on: picture-only style + text blocking; off: both removed"


def t_hold_y_street_view():
    js = ("(()=>{const b=document.querySelector('button[jsaction*=\"layerswitcher.intent.streetview\"]');"
          "return b?(b.getAttribute('aria-checked')||b.getAttribute('aria-pressed')):'x'})()")
    s = run(MAP_DAMRAK, [[0, "action", "street_view_hold"], [3000, "hold_end", "street_view"],
                         [12000, "action", "street_view_hold"]], sample=js, seconds=32)
    cov = [x["v"] for x in s if 11500 <= x["t"] <= 13000]
    assert "true" in cov, f"blue lines while holding: {cov}"
    sv = [x for x in s if 13000 <= x["t"] <= 21500 and view(x["url"])]
    assert sv, "Street View did not open after releasing Y"
    d_aim = dist((52.37555, 4.89620), pos(sv[-1]["url"]))
    assert d_aim < 60, f"Street View opened {d_aim:.0f} m from the crosshair"
    maps = [x for x in s if x["t"] >= 26000 and not view(x["url"]) and pos(x["url"])]
    assert maps, "did not return to the map"
    d_exit = dist(pos(sv[-1]["url"]), pos(maps[-1]["url"]))
    assert d_exit < 5, f"map centred {d_exit:.1f} m from the exit spot"
    late_cov = [x["v"] for x in maps]
    assert "true" not in late_cov[-2:], f"blue lines still on after returning: {late_cov[-3:]}"
    return f"lines on while holding; Street View {d_aim:.0f} m from aim; back on map {d_exit:.1f} m from exit spot; lines off"


def t_keyboard():
    s = run(MAP_DAMRAK, [[0, "action", "search"], [3000, "kbnav", "close"]],
            sample="window.__spKb?__spKb.isOpen():'x'", seconds=17)
    opened = [x["v"] for x in s if 10800 <= x["t"] <= 12800]
    closed = [x["v"] for x in s if x["t"] >= 14000]
    assert True in opened, f"keyboard while open: {opened}"
    assert closed and not any(v is True for v in closed), f"keyboard after close: {closed}"
    return "opens and closes"


def t_quit_on_map():
    run(MAP_DAMRAK, [[0, "action", "immersive"]], seconds=14, keep=True)
    n = count_running()
    kill_all()
    assert n == 0, "still running after View on the map"
    return "View on the map quit the app"


def t_settings_page():
    js = r"""new Promise((done) => {
      const n0 = document.querySelectorAll('#profile option').length;
      const res = { buttons: document.querySelectorAll('#buttonMap select').length, profiles: n0,
                    nav: typeof sp.nav, vib: !!document.querySelector('[data-k=vibration_strength]') };
      window.ipc.postMessage(JSON.stringify({ cmd: 'profileNew' }));
      setTimeout(() => {
        const opts = [...document.querySelectorAll('#profile option')];
        res.afterNew = opts.length;
        const mine = opts.find((o) => o.value.startsWith('user:') && o.selected);
        if (mine) window.ipc.postMessage(JSON.stringify({ cmd: 'profileDelete', id: mine.value }));
        setTimeout(() => { res.afterDelete = document.querySelectorAll('#profile option').length; done(res); }, 1200);
      }, 1200);
    })"""
    open(os.path.join(TMP, "sp-e2e-ui.js"), "w").write(js)
    out = os.path.join(TMP, "streetpad-debug-ui.txt")
    if os.path.exists(out):
        os.remove(out)
    run(MAP_DAMRAK, [], seconds=16, extra=["--settings", "--debug-js-ui", WIN_TMP + r"\sp-e2e-ui.js"])
    r = json.loads(open(out, encoding="utf-8").read())["result"]["value"]
    assert r["buttons"] == 28, f"button selects {r['buttons']} (expected 14 tap + 14 hold)"
    assert r["profiles"] >= 4 and r["nav"] == "function" and r["vib"], str(r)
    assert r["afterNew"] == r["profiles"] + 1 and r["afterDelete"] == r["profiles"], f"profiles {r['profiles']} -> {r['afterNew']} -> {r['afterDelete']}"
    return f"28 button selects, {r['profiles']} profiles, create+delete OK"


def t_private_mode():
    original = reg_get()
    d = json.loads(original)
    d["login_mode"] = "private"
    reg_set(json.dumps(d))
    try:
        run(MAP_DAMRAK, [], seconds=12, keep=True)
        pid = ps("(Get-Process StreetPad-e2e).Id")
        exists = ps(f"Test-Path \"$env:TEMP\\StreetPad-private-{pid}\"")
        ps("Get-Process StreetPad-e2e | % { $_.CloseMainWindow() | Out-Null }; Start-Sleep 9")
        gone = ps(f"Test-Path \"$env:TEMP\\StreetPad-private-{pid}\"")
        kill_all()
    finally:
        reg_set(original)
    assert exists == "True", "private profile folder was not created"
    assert gone == "False", "private profile folder still there after closing"
    return "temporary profile created and wiped on close"


def t_single_instance():
    run(MAP_DAMRAK, [], seconds=9, keep=True)
    ps(f"Start-Process '{EXE}'; Start-Sleep 4")
    n = count_running()
    kill_all()
    assert n == 1, f"{n} instances running"
    return "second launch did not start a second copy"


SCENARIOS = [t_turn_around, t_snap_turns, t_level_view, t_walk, t_bookmarks, t_immersive,
             t_hold_y_street_view, t_keyboard, t_quit_on_map, t_settings_page, t_private_mode, t_single_instance]


def main():
    wanted = sys.argv[1:]
    todo = [f for f in SCENARIOS if not wanted or f.__name__[2:] in wanted]
    ps("Get-Process StreetPad -EA SilentlyContinue | % { $_.CloseMainWindow() | Out-Null }; Start-Sleep 4")
    kill_all()
    backup = reg_get()
    open(os.path.join(ROOT, "target", "settings-backup.json"), "w", encoding="utf-8").write(backup)
    subprocess.run(["cp", EXE_BUILD, os.path.join(TMP, "StreetPad-e2e.exe")], check=True)
    results = []
    try:
        for f in todo:
            reg_set(backup)  # every scenario starts from the user's real settings
            try:
                msg = f()
                results.append((f.__name__[2:], True, msg))
            except AssertionError as e:
                results.append((f.__name__[2:], False, str(e)))
            except Exception as e:
                results.append((f.__name__[2:], False, f"{type(e).__name__}: {e}"))
            print(("  PASS " if results[-1][1] else "  FAIL ") + results[-1][0] + ": " + results[-1][2], flush=True)
    finally:
        kill_all()
        reg_set(backup)
        restored = reg_get() == backup
        for f in ("StreetPad-e2e.exe", "sp-e2e-actions.json", "sp-e2e-sample.js", "sp-e2e-ui.js", "sp-e2e-reg.json",
                  "streetpad-samples.txt", "streetpad-debug-ui.txt"):
            try:
                os.remove(os.path.join(TMP, f))
            except OSError:
                pass
        ps(f"Start-Process '{DESKTOP_EXE}'")
        print(f"settings restored exactly: {restored}")
    failed = [r for r in results if not r[1]]
    print(f"{len(results) - len(failed)}/{len(results)} passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
