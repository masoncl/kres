#!/usr/bin/env python3
"""Index and search a `kres --export` findings tree.

Two modes, picked by mutually exclusive flags:

  findings-index.py --generate
      Walk every `<tag>/metadata.yaml` in the current directory, sort
      the rows by severity (high → medium → low → unknown) then by
      date ascending then by id, and write:
        * INDEX.md   — markdown table for in-tree browsing.
        * index.html — same table with client-side filters for
                       browser / GitHub Pages viewing.

  findings-index.py --search "<query>"
      Print a markdown table — same format as INDEX.md — covering only
      the rows the query matches. The query is a space-separated list
      of `key:value` clauses, AND-ed together. Recognised keys:
        severity:<high|medium|low|unknown>
        subsystem:<text>             — exact match (em-dash for blank)
        status:<text>                — exact match
        since:<YYYY-MM-DD>            — date >= since (undated rows
                                         excluded)
        regex:<pattern>               — case-insensitive regex over
                                         the row's text columns

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

    The per-finding folders live under a `findings/` subtree so the
    top of the export dir stays uncluttered (INDEX.md, index.html,
    this script, …). Old export trees without the subtree return an
    empty list.
    """
    rows = []
    findings_root = os.path.join(root, "findings")
    if not os.path.isdir(findings_root):
        return rows
    for name in sorted(os.listdir(findings_root)):
        path = os.path.join(findings_root, name)
        meta = os.path.join(path, "metadata.yaml")
        if not os.path.isdir(path) or not os.path.isfile(meta):
            continue
        with open(meta, encoding="utf-8") as f:
            yaml_text = f.read()
        subsystem = parse_top_level(yaml_text, "subsystem") or ""
        rows.append({
            "tag": name,
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
            "[`{id}`](findings/{tag}/FINDING.md) | {title} |".format(
                sev=sev,
                subsys=md_escape_cell(subsystem),
                date=date_display,
                status=r["status"],
                id=r["id"],
                tag=r["tag"],
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
            '<td><a href="findings/{}/FINDING.md"><code>{}</code></a></td>'
            .format(e(r["tag"]), e(r["id"]))
        )
        parts.append("<td>{}</td>".format(e(r["title"])))
        parts.append("</tr>")

    parts.append("</tbody></table>")
    parts.append(FILTER_SCRIPT)
    parts.append("</body></html>")
    return "\n".join(parts) + "\n"


_QUERY_KEYS = ("severity", "subsystem", "status", "since", "regex")


def parse_query(query):
    """Parse a search query into a {key: value} dict.

    Tokens are whitespace-separated `key:value` pairs. Unknown keys
    or empty values raise ValueError so the operator sees their typo
    instead of silently getting the unfiltered list back.
    """
    clauses = {}
    for tok in query.split():
        if ":" not in tok:
            raise ValueError("clause without ':' — {!r}".format(tok))
        key, _, value = tok.partition(":")
        if key not in _QUERY_KEYS:
            raise ValueError(
                "unknown key {!r} (allowed: {})".format(
                    key, ", ".join(_QUERY_KEYS)
                )
            )
        if not value:
            raise ValueError("empty value for {}:".format(key))
        clauses[key] = value
    return clauses


def _row_haystack(r):
    sev = r["severity"] if r["severity"] in SEV_RANK else "?"
    date_display = r["date"] or "—"
    subsystem = r["subsystem"] if r["subsystem"] else "—"
    return " ".join(
        [sev, subsystem, date_display, r["status"], r["id"], r["title"]]
    )


def filter_rows(rows, clauses):
    sev_q = clauses.get("severity")
    # `severity:unknown` must match rows whose severity isn't one of
    # high/medium/low — those render as "?" in the table.
    if sev_q == "unknown":
        sev_q = "?"
    subsys_q = clauses.get("subsystem")
    status_q = clauses.get("status")
    since_q = clauses.get("since")
    pattern = None
    if "regex" in clauses:
        try:
            pattern = re.compile(clauses["regex"], re.IGNORECASE)
        except re.error as exc:
            raise ValueError("regex invalid: {}".format(exc))
    out = []
    for r in rows:
        sev_eff = r["severity"] if r["severity"] in SEV_RANK else "?"
        if sev_q and sev_eff != sev_q:
            continue
        sub_eff = r["subsystem"] if r["subsystem"] else "—"
        if subsys_q and sub_eff != subsys_q:
            continue
        if status_q and r["status"] != status_q:
            continue
        if since_q:
            if not r["date"] or r["date"] < since_q:
                continue
        if pattern and not pattern.search(_row_haystack(r)):
            continue
        out.append(r)
    return out


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
        clauses = parse_query(args.search)
        filtered = filter_rows(rows, clauses)
    except ValueError as exc:
        print("search: {}".format(exc), file=sys.stderr)
        return 2
    sys.stdout.write(build_markdown(filtered))
    return 0


if __name__ == "__main__":
    sys.exit(main() or 0)
