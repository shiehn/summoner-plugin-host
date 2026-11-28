#!/usr/bin/env python3
"""The public-API surface snapshot of a crate, from rustdoc JSON, as a sorted text list (one item per line).

    tools/public-api.py gen   <crate>...   write api/<crate>.txt
    tools/public-api.py check <crate>...   regenerate into a temporary dir and fail (exit 1) on any drift
    tools/public-api.py render <file.json> print the list for an existing rustdoc JSON file

With no crate, `gen` and `check` use the crates listed in api/CRATES (one package name per line). The JSON comes
from the installed nightly (`cargo +nightly rustdoc -p <crate> --lib -- -Z unstable-options --output-format json
--document-hidden-items`, into $CARGO_TARGET_DIR or target/), so a snapshot is the surface on the host target (the
header names it): the snapshots are the aarch64-apple-darwin surface, and another target's cfg surface gets its own
snapshots when its port lands. `api/NIGHTLY` names the nightly they were made with; a test
(`tests/public_api.rs` of a crate of this repo) runs `check` whenever that nightly is installed, and skips loudly
otherwise. Run it from the repository root; it builds nothing but the docs. Rustdoc
JSON's schema moves between nightlies: the header pins `format_version`, and `check` refuses a JSON of another
version instead of reporting a false drift (regenerate deliberately, in its own change, after a nightly update).

What a line names: every reachable public item by its public path (modules, re-exports, structs and their public
fields, enums and their variants, traits and their items, functions with their signatures, constants with their
values, statics, type aliases, macros), the inherent methods and associated consts of each public type, the traits
it implements (blanket impls are skipped; of the auto traits only Send and Sync are kept), and the attributes that
change what a caller may do (`#[non_exhaustive]`, `#[repr]`). A struct whose fields are not all public says so
(`{private fields}`). Docs and spans are not part of the surface. `#[doc(hidden)]` items are listed and marked: hidden
is still callable, so it is still surface.
"""

import json
import os
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
API_DIR = os.path.join(ROOT, "api")
AUTO_KEPT = {"Send", "Sync"}
# Compiler-internal marker impls (derive(PartialEq) adds one): not a caller's surface.
SKIPPED_TRAITS = {"StructuralPartialEq"}


def die(msg):
    sys.stderr.write("public-api: " + msg + "\n")
    sys.exit(2)


class Render:
    def __init__(self, doc):
        self.doc = doc
        self.index = doc["index"]
        self.paths = doc["paths"]
        self.lines = set()
        self.seen = set()

    def item(self, i):
        return self.index.get(str(i))

    def path_of(self, rid, written):
        p = self.paths.get(str(rid))
        if p and p.get("path"):
            return "::".join(p["path"])
        return written

    # ---- types -------------------------------------------------------------------------------------------------
    def args(self, a):
        if a is None:
            return ""
        if "angle_bracketed" in a:
            ab = a["angle_bracketed"]
            parts = []
            for g in ab.get("args", []):
                if "lifetime" in g:
                    parts.append(g["lifetime"])
                elif "type" in g:
                    parts.append(self.ty(g["type"]))
                elif "const" in g:
                    c = g["const"]
                    parts.append(c.get("expr") or c.get("value") or "_")
                elif g == "infer" or "infer" in g:
                    parts.append("_")
            for c in ab.get("constraints", []):
                name = c["name"] + self.args(c.get("args"))
                b = c["binding"]
                if "equality" in b:
                    t = b["equality"]
                    parts.append(name + " = " + (self.ty(t["type"]) if "type" in t else str(t.get("constant"))))
                else:
                    parts.append(name + ": " + self.bounds(b.get("constraint", [])))
            return "<" + ", ".join(parts) + ">" if parts else ""
        if "parenthesized" in a:
            pa = a["parenthesized"]
            out = "(" + ", ".join(self.ty(t) for t in pa["inputs"]) + ")"
            if pa.get("output") is not None:
                out += " -> " + self.ty(pa["output"])
            return out
        if a == "return_type_notation" or "return_type_notation" in a:
            return "(..)"
        return ""

    def rpath(self, rp):
        return self.path_of(rp.get("id"), rp["path"]) + self.args(rp.get("args"))

    def bounds(self, bs):
        out = []
        for b in bs:
            if "trait_bound" in b:
                tb = b["trait_bound"]
                mod = {"none": "", "maybe": "?", "maybe_const": "~const "}.get(tb.get("modifier", "none"), "")
                hr = self.hrtb(tb.get("generic_params", []))
                out.append(hr + mod + self.rpath(tb["trait"]))
            elif "outlives" in b:
                out.append(b["outlives"])
            elif "use" in b:
                out.append("use<" + ", ".join(self.precise(u) for u in b["use"]) + ">")
        return " + ".join(out)

    def precise(self, u):
        if isinstance(u, dict):
            return u.get("lifetime") or u.get("param") or str(u)
        return str(u)

    def hrtb(self, params):
        if not params:
            return ""
        return "for<" + ", ".join(p["name"] for p in params) + "> "

    def ty(self, t):
        if t is None:
            return "()"
        k = next(iter(t))
        v = t[k]
        if k == "resolved_path":
            return self.rpath(v)
        if k == "generic":
            return v
        if k == "primitive":
            return v
        if k == "tuple":
            return "(" + ", ".join(self.ty(x) for x in v) + ("," if len(v) == 1 else "") + ")"
        if k == "slice":
            return "[" + self.ty(v) + "]"
        if k == "array":
            return "[" + self.ty(v["type"]) + "; " + str(v["len"]) + "]"
        if k == "borrowed_ref":
            lt = (v["lifetime"] + " ") if v.get("lifetime") else ""
            return "&" + lt + ("mut " if v["is_mutable"] else "") + self.ty(v["type"])
        if k == "raw_pointer":
            return "*" + ("mut " if v["is_mutable"] else "const ") + self.ty(v["type"])
        if k == "impl_trait":
            return "impl " + self.bounds(v)
        if k == "dyn_trait":
            parts = [self.hrtb(p.get("generic_params", [])) + self.rpath(p["trait"]) for p in v["traits"]]
            if v.get("lifetime"):
                parts.append(v["lifetime"])
            return "dyn " + " + ".join(parts)
        if k == "qualified_path":
            st = self.ty(v["self_type"])
            if v.get("trait"):
                st = "<" + st + " as " + self.rpath(v["trait"]) + ">"
            return st + "::" + v["name"] + self.args(v.get("args"))
        if k == "function_pointer":
            return self.fnptr(v)
        if k == "infer":
            return "_"
        if k == "pat":
            return self.ty(v["type"]) + " is " + v.get("__pat_unstable_do_not_use", "_")
        return "<" + k + ">"

    def fnptr(self, v):
        h = self.header(v.get("header", {}))
        sig = v["sig"]
        ins = ", ".join(self.ty(x[1]) for x in sig["inputs"])
        out = "" if sig.get("output") is None else " -> " + self.ty(sig["output"])
        return self.hrtb(v.get("generic_params", [])) + h + "fn(" + ins + ")" + out

    def header(self, h):
        s = ""
        if h.get("is_const"):
            s += "const "
        if h.get("is_async"):
            s += "async "
        if h.get("is_unsafe"):
            s += "unsafe "
        abi = h.get("abi", "Rust")
        if abi != "Rust":
            if isinstance(abi, dict):
                name = next(iter(abi))
                s += 'extern "' + name.replace("_", "-").upper().replace("C-UNWIND", "C-unwind") + '" '
            else:
                s += 'extern "' + str(abi) + '" '
        return s

    def generics(self, g):
        if not g:
            return "", ""
        ps = []
        for p in g.get("params", []):
            kind = p["kind"]
            if "lifetime" in kind:
                o = kind["lifetime"].get("outlives", [])
                ps.append(p["name"] + (": " + " + ".join(o) if o else ""))
            elif "type" in kind:
                if kind["type"].get("is_synthetic"):
                    continue
                b = self.bounds(kind["type"].get("bounds", []))
                d = kind["type"].get("default")
                ps.append(p["name"] + (": " + b if b else "") + (" = " + self.ty(d) if d else ""))
            elif "const" in kind:
                ps.append("const " + p["name"] + ": " + self.ty(kind["const"]["type"]))
        wh = []
        for w in g.get("where_predicates", []):
            if "bound_predicate" in w:
                bp = w["bound_predicate"]
                wh.append(self.hrtb(bp.get("generic_params", [])) + self.ty(bp["type"]) + ": "
                          + self.bounds(bp["bounds"]))
            elif "lifetime_predicate" in w:
                lp = w["lifetime_predicate"]
                wh.append(lp["lifetime"] + ": " + " + ".join(lp["outlives"]))
            elif "eq_predicate" in w:
                ep = w["eq_predicate"]
                wh.append(self.ty(ep["lhs"]) + " = " + str(ep["rhs"]))
        return ("<" + ", ".join(ps) + ">" if ps else ""), (" where " + ", ".join(wh) if wh else "")

    def fn_sig(self, f):
        sig = f["sig"]
        ins = []
        for name, t in sig["inputs"]:
            if name == "self":
                ins.append(self.selfarg(t))
            else:
                ins.append(name + ": " + self.ty(t))
        if sig.get("is_c_variadic"):
            ins.append("...")
        out = "" if sig.get("output") is None else " -> " + self.ty(sig["output"])
        g, wh = self.generics(f.get("generics"))
        return self.header(f.get("header", {})), g, "(" + ", ".join(ins) + ")" + out + wh

    def selfarg(self, t):
        if t == {"generic": "Self"}:
            return "self"
        if "borrowed_ref" in t and t["borrowed_ref"]["type"] == {"generic": "Self"}:
            r = t["borrowed_ref"]
            lt = (r["lifetime"] + " ") if r.get("lifetime") else ""
            return "&" + lt + ("mut " if r["is_mutable"] else "") + "self"
        return "self: " + self.ty(t)

    # ---- attributes --------------------------------------------------------------------------------------------
    def attrs(self, it):
        out = []
        for a in it.get("attrs", []) or []:
            if a == "non_exhaustive":
                out.append("#[non_exhaustive]")
            elif isinstance(a, dict) and "repr" in a:
                r = a["repr"]
                parts = []
                if r.get("kind") and r["kind"] != "rust":
                    parts.append(r["kind"].upper() if r["kind"] == "c" else r["kind"])
                if r.get("int"):
                    parts.append(r["int"])
                if r.get("align"):
                    parts.append("align(" + str(r["align"]) + ")")
                if r.get("packed"):
                    parts.append("packed(" + str(r["packed"]) + ")")
                if parts:
                    out.append("#[repr(" + ", ".join(parts) + ")]")
            elif isinstance(a, dict) and "must_use" in a:
                out.append("#[must_use]")
            elif isinstance(a, dict) and a.get("other") == "#[doc(hidden)]":
                out.append("#[doc(hidden)]")
        return (" ".join(out) + " ") if out else ""

    # ---- the walk ----------------------------------------------------------------------------------------------
    def add(self, s):
        self.lines.add(s)

    def module(self, mid, path):
        m = self.item(mid)
        if m is None:
            return
        key = ("mod", mid, path)
        if key in self.seen:
            return
        self.seen.add(key)
        for cid in m["inner"]["module"]["items"]:
            c = self.item(cid)
            if c is None:
                continue
            self.entry(c, path)

    def visible(self, it):
        return it.get("visibility") == "public"

    def entry(self, it, path):
        k = next(iter(it["inner"]))
        v = it["inner"][k]
        if k == "use":
            if not self.visible(it):
                return
            name = v["name"]
            if v.get("is_glob"):
                self.add("pub use " + path + "::" + v["source"] + "::*")
                tgt = self.item(v["id"]) if v.get("id") is not None else None
                if tgt is not None and "module" in tgt["inner"]:
                    self.module(v["id"], path)
                return
            self.add("pub use " + path + "::" + name + " = " + v["source"])
            tgt = self.item(v["id"]) if v.get("id") is not None else None
            if tgt is not None and tgt.get("crate_id") == 0:
                t2 = dict(tgt)
                t2["name"] = name
                t2["visibility"] = "public"
                self.entry(t2, path)
            return
        if k in ("impl",):
            return
        if not self.visible(it):
            return
        name = it["name"]
        p = path + "::" + name
        a = self.attrs(it)
        if k == "module":
            self.add("pub mod " + p)
            self.module(it["id"], p)
        elif k == "struct":
            g, wh = self.generics(v.get("generics"))
            kind = v["kind"]
            if kind == "unit":
                self.add(a + "pub struct " + p + g + wh + ";")
            elif "tuple" in kind:
                fs = kind["tuple"]
                shown = []
                for n, fid in enumerate(fs):
                    f = self.item(fid) if fid is not None else None
                    if f is not None and self.visible(f):
                        shown.append("pub " + self.ty(f["inner"]["struct_field"]))
                    else:
                        shown.append("_")
                self.add(a + "pub struct " + p + g + "(" + ", ".join(shown) + ")" + wh + ";")
            else:
                pl = kind["plain"]
                priv = " {private fields}" if pl.get("has_stripped_fields") else ""
                self.add(a + "pub struct " + p + g + wh + priv)
                for fid in pl["fields"]:
                    f = self.item(fid)
                    if f is not None and self.visible(f):
                        self.add("pub field " + p + "::" + f["name"] + ": " + self.ty(f["inner"]["struct_field"]))
            self.impls(v.get("impls", []), p)
        elif k == "union":
            g, wh = self.generics(v.get("generics"))
            self.add(a + "pub union " + p + g + wh)
            for fid in v.get("fields", []):
                f = self.item(fid)
                if f is not None and self.visible(f):
                    self.add("pub field " + p + "::" + f["name"] + ": " + self.ty(f["inner"]["struct_field"]))
            self.impls(v.get("impls", []), p)
        elif k == "enum":
            g, wh = self.generics(v.get("generics"))
            self.add(a + "pub enum " + p + g + wh)
            for vid in v["variants"]:
                var = self.item(vid)
                if var is None:
                    continue
                self.variant(var, p)
            self.impls(v.get("impls", []), p)
        elif k == "function":
            h, g, rest = self.fn_sig(v)
            self.add(a + "pub " + h + "fn " + p + g + rest)
        elif k == "constant":
            c = v["const"]
            self.add("pub const " + p + ": " + self.ty(v["type"]) + " = " + str(c.get("expr")))
        elif k == "static":
            self.add("pub static " + ("mut " if v.get("is_mutable") else "") + p + ": " + self.ty(v["type"]))
        elif k == "type_alias":
            g, wh = self.generics(v.get("generics"))
            self.add("pub type " + p + g + " = " + self.ty(v["type"]) + wh)
        elif k == "trait":
            g, wh = self.generics(v.get("generics"))
            b = self.bounds(v.get("bounds", []))
            un = "unsafe " if v.get("is_unsafe") else ""
            auto = "auto " if v.get("is_auto") else ""
            dc = "" if v.get("is_dyn_compatible", v.get("is_object_safe", True)) else " {not dyn-compatible}"
            self.add(a + "pub " + un + auto + "trait " + p + g + (": " + b if b else "") + wh + dc)
            for iid in v["items"]:
                ti = self.item(iid)
                if ti is not None:
                    self.assoc(ti, p, in_trait=True)
            for iid in v.get("implementations", []):
                im = self.item(iid)
                if im is None:
                    continue
                ii = im["inner"]["impl"]
                if ii.get("blanket_impl") is not None:
                    continue
                self.add("impl " + p + self.args(ii["trait"].get("args")) + " for " + self.ty(ii["for"]))
        elif k == "macro":
            self.add("pub macro " + p + "!")
        elif k == "proc_macro":
            self.add("pub proc_macro " + p + "!")
        elif k == "trait_alias":
            self.add("pub trait alias " + p)
        else:
            self.add("pub " + k + " " + p)

    def variant(self, var, ep):
        vk = var["inner"]["variant"]
        a = self.attrs(var)
        p = ep + "::" + var["name"]
        kind = vk["kind"]
        disc = vk.get("discriminant")
        d = (" = " + disc["expr"]) if disc else ""
        if kind == "plain":
            self.add(a + "pub variant " + p + d)
        elif "tuple" in kind:
            shown = []
            for fid in kind["tuple"]:
                f = self.item(fid) if fid is not None else None
                shown.append(self.ty(f["inner"]["struct_field"]) if f is not None else "_")
            self.add(a + "pub variant " + p + "(" + ", ".join(shown) + ")" + d)
        else:
            st = kind["struct"]
            priv = " {private fields}" if st.get("has_stripped_fields") else ""
            self.add(a + "pub variant " + p + " {}" + priv + d)
            for fid in st["fields"]:
                f = self.item(fid)
                if f is not None:
                    self.add("pub field " + p + "::" + f["name"] + ": " + self.ty(f["inner"]["struct_field"]))

    def assoc(self, ti, owner, in_trait):
        k = next(iter(ti["inner"]))
        v = ti["inner"][k]
        if not in_trait and not self.visible(ti):
            return
        p = owner + "::" + ti["name"]
        a = self.attrs(ti)
        if k == "function":
            h, g, rest = self.fn_sig(v)
            body = " {provided}" if in_trait and v.get("has_body") else ""
            self.add(a + "pub " + h + "fn " + p + g + rest + body)
        elif k == "assoc_const":
            self.add("pub const " + p + ": " + self.ty(v["type"]) + ((" = " + v["value"]) if v.get("value") else ""))
        elif k == "assoc_type":
            b = self.bounds(v.get("bounds", []))
            t = (" = " + self.ty(v["type"])) if v.get("type") else ""
            self.add("pub type " + p + (": " + b if b else "") + t)

    def impls(self, ids, p):
        for iid in ids:
            im = self.item(iid)
            if im is None:
                continue
            ii = im["inner"]["impl"]
            if ii.get("blanket_impl") is not None:
                continue
            g, wh = self.generics(ii.get("generics"))
            neg = "!" if ii.get("is_negative") else ""
            if ii.get("trait") is None:
                for aid in ii["items"]:
                    ai = self.item(aid)
                    if ai is not None:
                        self.assoc(ai, p, in_trait=False)
                continue
            tname = ii["trait"]["path"].split("::")[-1]
            if ii.get("is_synthetic") and tname not in AUTO_KEPT:
                continue
            if tname in SKIPPED_TRAITS:
                continue
            un = "unsafe " if ii.get("is_unsafe") else ""
            self.add(un + "impl" + g + " " + neg + self.rpath(ii["trait"]) + " for " + self.ty(ii["for"]) + wh)

    def run(self):
        root = self.item(self.doc["root"])
        name = root["name"]
        self.module(self.doc["root"], name)
        return sorted(self.lines)


def render(doc, crate):
    lines = Render(doc).run()
    head = [
        "# The public API of " + crate + " (tools/public-api.py; rustdoc JSON format_version "
        + str(doc["format_version"]) + ", target " + str(doc.get("target", {}).get("triple", "host")) + ").",
        "# Regenerate: tools/public-api.py gen " + crate + "; check: tools/public-api.py check.",
    ]
    return "\n".join(head + lines) + "\n"


def crates_from_list():
    f = os.path.join(API_DIR, "CRATES")
    with open(f) as fh:
        return [ln.strip() for ln in fh if ln.strip() and not ln.startswith("#")]


def build_json(crate, tdir):
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = tdir
    cmd = ["cargo", "+nightly", "rustdoc", "-q", "-p", crate, "--lib", "--", "-Z", "unstable-options",
           "--output-format", "json", "--document-hidden-items"]
    r = subprocess.run(cmd, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        die("rustdoc JSON failed for " + crate + ":\n" + r.stderr[-4000:])
    path = os.path.join(tdir, "doc", crate.replace("-", "_") + ".json")
    with open(path) as fh:
        return json.load(fh)


def header_version(text):
    first = text.split("\n", 1)[0]
    marker = "format_version "
    if marker not in first:
        return None
    return first.split(marker, 1)[1].split(",", 1)[0]


def main(argv):
    if len(argv) < 2 or argv[1] not in ("gen", "check", "render"):
        die("usage: public-api.py gen|check [crate...] | render <file.json> <crate>")
    mode = argv[1]
    if mode == "render":
        with open(argv[2]) as fh:
            sys.stdout.write(render(json.load(fh), argv[3] if len(argv) > 3 else "?"))
        return 0
    crates = argv[2:] or crates_from_list()
    tdir = os.environ.get("CARGO_TARGET_DIR") or os.path.join(ROOT, "target")
    drift = 0
    for c in crates:
        doc = build_json(c, tdir)
        text = render(doc, c)
        out = os.path.join(API_DIR, c + ".txt")
        if mode == "gen":
            os.makedirs(API_DIR, exist_ok=True)
            with open(out, "w") as fh:
                fh.write(text)
            print("wrote api/" + c + ".txt (" + str(text.count("\n") - 2) + " items)")
            continue
        if not os.path.exists(out):
            print("DRIFT " + c + ": no committed snapshot api/" + c + ".txt")
            drift += 1
            continue
        with open(out) as fh:
            old = fh.read()
        ov = header_version(old)
        if ov is not None and ov != str(doc["format_version"]):
            die(c + ": the snapshot is rustdoc JSON format_version " + ov + ", the installed nightly writes "
                + str(doc["format_version"]) + "; regenerate on purpose (gen), in its own change")
        if old != text:
            drift += 1
            with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as tf:
                tf.write(text)
            d = subprocess.run(["diff", "-u", out, tf.name], stdout=subprocess.PIPE, text=True).stdout
            os.unlink(tf.name)
            print("DRIFT " + c + ":\n" + d)
        else:
            print("ok    " + c)
    if drift:
        print(str(drift) + " crate(s) drifted from api/: review the diff, then tools/public-api.py gen <crate>")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
