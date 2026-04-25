#!/usr/bin/env python3
"""Index and search a `kres --export` findings tree.

Two modes, picked by mutually exclusive flags:

  findings-index.py --generate
      Walk every `<tag>/metadata.yaml` under the current directory and
      write:
        * INDEX.md   — markdown table covering all findings.
        * index.html — same table with client-side filters for
                       browser / GitHub Pages viewing.
      Use `--search subsystem:<regex>` to pivot a large tree by area
      without scanning the whole list.

  findings-index.py --search "<query>"
      Print a markdown table — same format as INDEX.md — covering only
      the rows the query matches. The query is a boolean expression
      over `key:value` clauses:

        clause     := KEY ':' REGEX
        and-expr   := clause ( ('-a')? clause )*
        or-expr    := and-expr ( '-o' and-expr )*
        atom       := '(' or-expr ')' | clause

      `-a` (and) is the default operator between adjacent clauses;
      `-o` (or) is explicit. Parens group sub-expressions and must be
      whitespace-separated (use `[(]` / `[)]` inside a regex value).

      All clause values are case-insensitive regular expressions over
      the matching column, except `since:` which is a date bound:

        severity:<regex>      — matches against {high,medium,low,?}
        subsystem:<regex>     — matches against the row's subsystem
                                ("—" when blank)
        status:<regex>        — matches against the row's status
        regex:<regex>         — matches against the joined row text
                                (sev + subsystem + date + status +
                                id + title)
        since:<YYYY-MM-DD>    — date >= since (undated rows excluded)

A copy of this script is installed alongside the exported findings the
first time `kres --export` (or `--export-index`) runs over a directory.
Subsequent runs do not overwrite it — edit it freely to customise the
layout, filters, or styling.
"""

import argparse
import datetime
import html
import os
import re
import sys


SEV_RANK = {"high": 3, "medium": 2, "low": 1}


def parse_top_level(yaml_text, key):
    """Return the value of a top-level scalar key, or None.

    Mirrors the kres Rust top_level_scalar parser: ignores indented
    lines (nested mappings / list items) and unwraps a few backslash
    escapes inside double-quoted values. Good enough for the
    metadata.yaml shape kres emits.
    """
    needle = key + ": "
    for line in yaml_text.splitlines():
        if line.startswith(" ") or line.startswith("\t"):
            continue
        if not line.startswith(needle):
            continue
        rest = line[len(needle):].strip()
        if len(rest) >= 2 and rest.startswith('"') and rest.endswith('"'):
            return _unquote(rest[1:-1])
        return rest
    return None


_ESCAPES = {
    "\\\\": "\\",
    '\\"': '"',
    "\\n": "\n",
    "\\r": "\r",
    "\\t": "\t",
}


def _unquote(s):
    out = []
    i = 0
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s):
            pair = s[i:i + 2]
            out.append(_ESCAPES.get(pair, s[i + 1]))
            i += 2
        else:
            out.append(s[i])
            i += 1
    return "".join(out)


def collect_rows(root):
    """Walk `<root>/findings/<tag>/metadata.yaml` for every finding.

    Prefers the modern `<root>/findings/<tag>/` layout. Falls back to
    the pre-`findings/` flat layout (`<root>/<tag>/`) when the
    `findings/` subdir doesn't exist, so legacy export trees and
    operator-curated bug roots still produce an index. The per-row
    `tag_path` field records which relative subdirectory the metadata
    came from so the markdown / html row-link form points at the
    correct FINDING.md path.
    """
    findings_root = os.path.join(root, "findings")
    if os.path.isdir(findings_root):
        scan_root = findings_root
        tag_prefix = "findings/"
    else:
        scan_root = root
        tag_prefix = ""

    rows = []
    for name in sorted(os.listdir(scan_root)):
        if name == ".git":
            continue
        path = os.path.join(scan_root, name)
        meta = os.path.join(path, "metadata.yaml")
        if not os.path.isdir(path) or not os.path.isfile(meta):
            continue
        with open(meta, encoding="utf-8") as f:
            yaml_text = f.read()
        subsystem = parse_top_level(yaml_text, "subsystem") or ""
        rows.append({
            "tag": name,
            "tag_path": tag_prefix + name,
            "id": parse_top_level(yaml_text, "id") or "",
            "title": parse_top_level(yaml_text, "title") or "",
            "severity": (parse_top_level(yaml_text, "severity") or "").strip(),
            "status": parse_top_level(yaml_text, "status") or "active",
            "date": parse_top_level(yaml_text, "date"),
            "subsystem": subsystem if subsystem else None,
        })
    return rows


def sort_rows(rows):
    # Severity desc, undated rows last within their tier, date asc,
    # then id for determinism.
    rows.sort(key=lambda r: (
        -SEV_RANK.get(r["severity"], 0),
        r["date"] is None,
        r["date"] or "",
        r["id"],
    ))


def md_escape_cell(s):
    """Pipes break GFM table cells; newlines break the row.

    Mirror the kres Rust escape_md_table_cell helper so INDEX.md keeps
    its earlier structure exactly: a `|` becomes `\\|` and any newline
    is collapsed to a single space.
    """
    return s.replace("|", "\\|").replace("\n", " ")


def build_markdown(rows):
    parts = []
    parts.append("# kres findings index")
    parts.append("")
    ts = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
    parts.append("_generated: {}_".format(ts))
    parts.append("")
    if not rows:
        parts.append("(no findings)")
        return "\n".join(parts) + "\n"

    counts = {"high": 0, "medium": 0, "low": 0, "unknown": 0}
    for r in rows:
        sev = r["severity"]
        counts[sev if sev in counts else "unknown"] += 1
    summary = "{} finding(s): {} high, {} medium, {} low".format(
        len(rows), counts["high"], counts["medium"], counts["low"]
    )
    if counts["unknown"]:
        summary += ", {} unknown-severity".format(counts["unknown"])
    parts.append(summary)
    parts.append("")
    parts.append("| Severity | Subsystem | Date | Status | ID | Title |")
    parts.append("|---|---|---|---|---|---|")
    for r in rows:
        sev = r["severity"] if r["severity"] in SEV_RANK else "?"
        date_display = r["date"] or "—"
        subsystem = r["subsystem"] if r["subsystem"] else "—"
        parts.append(
            "| {sev} | {subsys} | {date} | {status} | "
            "[`{id}`]({tag_path}/FINDING.md) | {title} |".format(
                sev=sev,
                subsys=md_escape_cell(subsystem),
                date=date_display,
                status=r["status"],
                id=r["id"],
                tag_path=r["tag_path"],
                title=md_escape_cell(r["title"]),
            )
        )
    return "\n".join(parts) + "\n"


FILTER_SCRIPT = """<script>
(function () {
  var rows = Array.prototype.slice.call(
    document.querySelectorAll('tbody tr.findings-row')
  );
  var sevSelect = document.getElementById('filter-severity');
  var subsysSelect = document.getElementById('filter-subsystem');
  var statusSelect = document.getElementById('filter-status');
  var dateInput = document.getElementById('filter-date');
  var searchInput = document.getElementById('filter-search');
  var resetBtn = document.getElementById('filter-reset');
  var counter = document.getElementById('row-counter');

  // Populate the subsystem dropdown from distinct row values, sorted.
  // Skip the em-dash placeholder — there's nothing useful to filter
  // on for unclassified rows.
  var subsystems = {};
  rows.forEach(function (r) {
    var s = r.dataset.subsystem || '';
    if (s && s !== '\\u2014') subsystems[s] = true;
  });
  Object.keys(subsystems).sort().forEach(function (s) {
    var o = document.createElement('option');
    o.value = s; o.textContent = s;
    subsysSelect.appendChild(o);
  });

  // Populate the status dropdown the same way — driven by what the
  // metadata.yaml files actually carry (active / invalidated today,
  // but a future status enum lands here automatically without an
  // index.html rebuild).
  var statuses = {};
  rows.forEach(function (r) {
    var s = r.dataset.status || '';
    if (s) statuses[s] = true;
  });
  Object.keys(statuses).sort().forEach(function (s) {
    var o = document.createElement('option');
    o.value = s; o.textContent = s;
    statusSelect.appendChild(o);
  });

  function applyFilters() {
    var sev = sevSelect.value;
    var subsys = subsysSelect.value;
    var status = statusSelect.value;
    var since = dateInput.value;
    var pattern = null, patternErr = false;
    var q = searchInput.value;
    if (q !== '') {
      try {
        pattern = new RegExp(q, 'i');
        searchInput.classList.remove('bad');
      } catch (e) {
        patternErr = true;
        searchInput.classList.add('bad');
      }
    } else {
      searchInput.classList.remove('bad');
    }
    var visible = 0;
    rows.forEach(function (row) {
      var show = true;
      if (sev && row.dataset.severity !== sev) show = false;
      if (show && subsys && row.dataset.subsystem !== subsys) show = false;
      if (show && status && row.dataset.status !== status) show = false;
      if (show && since) {
        // String compare works: dates are YYYY-MM-DD. Rows with no
        // date hide as soon as a since bound is set — we can't know
        // they're new enough.
        if (!row.dataset.date || row.dataset.date < since) show = false;
      }
      if (show && pattern && !pattern.test(row.dataset.text)) show = false;
      if (show && patternErr) show = false;
      row.style.display = show ? '' : 'none';
      if (show) visible++;
    });
    counter.textContent = visible + ' / ' + rows.length + ' visible';
  }

  function resetFilters() {
    sevSelect.value = '';
    subsysSelect.value = '';
    statusSelect.value = '';
    dateInput.value = '';
    searchInput.value = '';
    searchInput.classList.remove('bad');
    applyFilters();
  }

  sevSelect.addEventListener('change', applyFilters);
  subsysSelect.addEventListener('change', applyFilters);
  statusSelect.addEventListener('change', applyFilters);
  dateInput.addEventListener('change', applyFilters);
  searchInput.addEventListener('input', applyFilters);
  resetBtn.addEventListener('click', resetFilters);
  applyFilters();
})();
</script>
"""


def build_html(rows):
    e = html.escape
    parts = []
    parts.append("<!doctype html>")
    parts.append('<html lang="en">')
    parts.append("<head>")
    parts.append('<meta charset="utf-8">')
    parts.append("<title>kres findings index</title>")
    parts.append("<style>")
    parts.append("body { font-family: sans-serif; margin: 2em; }")
    parts.append("table { border-collapse: collapse; }")
    parts.append("th, td { border: 1px solid #ccc; padding: 4px 8px; "
                 "text-align: left; vertical-align: top; }")
    parts.append("th { background: #f4f4f4; }")
    parts.append("code { font-family: monospace; }")
    parts.append(".filters { display: flex; flex-wrap: wrap; "
                 "gap: 0.75em; align-items: center; margin: 1em 0; }")
    parts.append(".filters label { display: flex; flex-direction: column; "
                 "font-size: 0.85em; color: #555; }")
    parts.append(".filters input, .filters select { font: inherit; "
                 "padding: 2px 4px; }")
    parts.append(".filters input.bad { background: #fee; }")
    parts.append(".filters button { font: inherit; padding: 2px 8px; }")
    parts.append("#row-counter { font-size: 0.85em; color: #555; "
                 "margin-left: auto; }")
    parts.append("</style></head><body>")
    parts.append("<h1>kres findings index</h1>")
    ts = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
    parts.append("<p><em>generated: {}</em></p>".format(e(ts)))

    if not rows:
        parts.append("<p>(no findings)</p></body></html>")
        return "\n".join(parts) + "\n"

    counts = {"high": 0, "medium": 0, "low": 0, "unknown": 0}
    for r in rows:
        sev = r["severity"]
        counts[sev if sev in counts else "unknown"] += 1
    summary = "{} finding(s): {} high, {} medium, {} low".format(
        len(rows), counts["high"], counts["medium"], counts["low"]
    )
    if counts["unknown"]:
        summary += ", {} unknown-severity".format(counts["unknown"])
    parts.append("<p>{}</p>".format(e(summary)))

    parts.append('<div class="filters">')
    parts.append(
        '<label>Severity<select id="filter-severity">'
        '<option value="">all</option>'
        '<option value="high">high</option>'
        '<option value="medium">medium</option>'
        '<option value="low">low</option>'
        '<option value="?">unknown</option>'
        '</select></label>'
    )
    parts.append(
        '<label>Subsystem<select id="filter-subsystem">'
        '<option value="">all</option></select></label>'
    )
    parts.append(
        '<label>Status<select id="filter-status">'
        '<option value="">all</option></select></label>'
    )
    parts.append('<label>Since<input id="filter-date" type="date"></label>')
    parts.append(
        '<label>Regex search<input id="filter-search" type="search" '
        'placeholder="e.g. uaf|race" size="30"></label>'
    )
    parts.append('<button id="filter-reset" type="button">reset</button>')
    parts.append('<span id="row-counter"></span>')
    parts.append("</div>")

    parts.append("<table><thead><tr>")
    for col in ["Severity", "Subsystem", "Date", "Status", "ID", "Title"]:
        parts.append("<th>{}</th>".format(col))
    parts.append("</tr></thead><tbody>")

    for r in rows:
        sev = r["severity"] if r["severity"] in SEV_RANK else "?"
        date_display = r["date"] or "—"
        date_attr = r["date"] or ""
        subsystem = r["subsystem"] if r["subsystem"] else "—"
        haystack = " ".join(
            [sev, subsystem, date_display, r["status"], r["id"], r["title"]]
        )
        parts.append(
            '<tr class="findings-row" data-severity="{sev}" '
            'data-subsystem="{subsys}" data-status="{status}" '
            'data-date="{date}" data-text="{haystack}">'.format(
                sev=e(sev),
                subsys=e(subsystem),
                status=e(r["status"]),
                date=e(date_attr),
                haystack=e(haystack),
            )
        )
        parts.append("<td>{}</td>".format(e(sev)))
        parts.append("<td>{}</td>".format(e(subsystem)))
        parts.append("<td>{}</td>".format(e(date_display)))
        parts.append("<td>{}</td>".format(e(r["status"])))
        parts.append(
            '<td><a href="{}/FINDING.md"><code>{}</code></a></td>'
            .format(e(r["tag_path"]), e(r["id"]))
        )
        parts.append("<td>{}</td>".format(e(r["title"])))
        parts.append("</tr>")

    parts.append("</tbody></table>")
    parts.append(FILTER_SCRIPT)
    parts.append("</body></html>")
    return "\n".join(parts) + "\n"


_QUERY_KEYS = ("severity", "subsystem", "status", "since", "regex")
_REGEX_KEYS = ("severity", "subsystem", "status", "regex")


def _row_haystack(r):
    sev = r["severity"] if r["severity"] in SEV_RANK else "?"
    date_display = r["date"] or "—"
    subsystem = r["subsystem"] if r["subsystem"] else "—"
    return " ".join(
        [sev, subsystem, date_display, r["status"], r["id"], r["title"]]
    )


# --- search expression AST + parser -----------------------------------
#
# Grammar:
#     or_expr  := and_expr ( '-o' and_expr )*
#     and_expr := atom ( ('-a')? atom )*       # implicit AND between atoms
#     atom     := '(' or_expr ')' | clause
#     clause   := KEY ':' VALUE
#
# All clause values are case-insensitive regular expressions over the
# matching column, except `since:` which compares as a date string.
# Stdlib only — no third-party expression library that fits the
# bundled-into-kres / no-pip-install constraint.


class _Clause:
    def __init__(self, key, value):
        self.key = key
        self.value = value
        if key in _REGEX_KEYS:
            try:
                self.pattern = re.compile(value, re.IGNORECASE)
            except re.error as exc:
                raise ValueError(
                    "invalid regex for {}: {}".format(key, exc)
                )
        else:
            self.pattern = None

    def matches(self, row):
        if self.key == "severity":
            sev_eff = row["severity"] if row["severity"] in SEV_RANK else "?"
            return self.pattern.search(sev_eff) is not None
        if self.key == "subsystem":
            sub_eff = row["subsystem"] if row["subsystem"] else "—"
            return self.pattern.search(sub_eff) is not None
        if self.key == "status":
            return self.pattern.search(row["status"]) is not None
        if self.key == "regex":
            return self.pattern.search(_row_haystack(row)) is not None
        if self.key == "since":
            return bool(row["date"]) and row["date"] >= self.value
        return False


class _And:
    def __init__(self, parts):
        self.parts = parts

    def matches(self, row):
        return all(p.matches(row) for p in self.parts)


class _Or:
    def __init__(self, parts):
        self.parts = parts

    def matches(self, row):
        return any(p.matches(row) for p in self.parts)


def _tokenize(query):
    """Whitespace-split, with `(` and `)` carved out as standalone
    tokens when they appear at a token's leading or trailing edge.

    A regex value that needs a literal paren must escape it with a
    character class — `regex:foo[(]bar` — because the tokenizer
    splits parens that bookend a token.
    """
    raw = query.split()
    tokens = []
    for t in raw:
        while t.startswith("("):
            tokens.append("(")
            t = t[1:]
        trailing = []
        while t.endswith(")"):
            trailing.append(")")
            t = t[:-1]
        if t:
            tokens.append(t)
        tokens.extend(trailing)
    return tokens


def _make_clause(token):
    if ":" not in token:
        raise ValueError("expected key:value, got {!r}".format(token))
    key, _, value = token.partition(":")
    if key not in _QUERY_KEYS:
        raise ValueError(
            "unknown key {!r} (allowed: {})".format(
                key, ", ".join(_QUERY_KEYS)
            )
        )
    if not value:
        raise ValueError("empty value for {}:".format(key))
    return _Clause(key, value)


def parse_query(query):
    """Parse a query string into an AST root node.

    The AST exposes a single `.matches(row)` method; callers feed
    each row from `collect_rows` and keep the rows where matches
    returns True.
    """
    tokens = _tokenize(query)
    pos = [0]

    def peek():
        return tokens[pos[0]] if pos[0] < len(tokens) else None

    def consume():
        t = tokens[pos[0]]
        pos[0] += 1
        return t

    def parse_or():
        parts = [parse_and()]
        while peek() == "-o":
            consume()
            parts.append(parse_and())
        return parts[0] if len(parts) == 1 else _Or(parts)

    def parse_and():
        parts = [parse_atom()]
        while peek() not in (None, ")", "-o"):
            if peek() == "-a":
                consume()
            parts.append(parse_atom())
        return parts[0] if len(parts) == 1 else _And(parts)

    def parse_atom():
        t = peek()
        if t is None:
            raise ValueError("unexpected end of expression")
        if t == "(":
            consume()
            inner = parse_or()
            if peek() != ")":
                raise ValueError("expected ')'")
            consume()
            return inner
        if t == ")":
            raise ValueError("unexpected ')'")
        if t in ("-a", "-o"):
            raise ValueError(
                "operator {!r} without a left-hand clause".format(t)
            )
        return _make_clause(consume())

    if not tokens:
        raise ValueError("empty query")
    expr = parse_or()
    if peek() is not None:
        raise ValueError("trailing tokens: {!r}".format(tokens[pos[0]:]))
    return expr


def filter_rows(rows, expr):
    """Return rows where `expr.matches(row)` is True."""
    return [r for r in rows if expr.matches(r)]




def main():
    parser = argparse.ArgumentParser(
        prog="findings-index.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--generate",
        action="store_true",
        help="walk cwd and write INDEX.md + index.html",
    )
    mode.add_argument(
        "--search",
        metavar="QUERY",
        help="print a filtered markdown table to stdout (see module "
             "doc for query syntax)",
    )
    args = parser.parse_args()

    root = os.getcwd()
    rows = collect_rows(root)
    sort_rows(rows)

    if args.generate:
        md_path = os.path.join(root, "INDEX.md")
        with open(md_path, "w", encoding="utf-8") as f:
            f.write(build_markdown(rows))
        html_path = os.path.join(root, "index.html")
        with open(html_path, "w", encoding="utf-8") as f:
            f.write(build_html(rows))
        print(
            "wrote {} and {} ({} row(s))".format(
                md_path, html_path, len(rows)
            ),
            file=sys.stderr,
        )
        return 0

    # --search QUERY
    try:
        expr = parse_query(args.search)
        filtered = filter_rows(rows, expr)
    except ValueError as exc:
        print("search: {}".format(exc), file=sys.stderr)
        return 2
    sys.stdout.write(build_markdown(filtered))
    return 0


if __name__ == "__main__":
    sys.exit(main() or 0)
