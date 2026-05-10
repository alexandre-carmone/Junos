#!/usr/bin/env python3
"""Strip Trunk directives from index.html and inject the WASM init script.

CSS `<link data-trunk rel="css" href="X"/>` lines are rewritten to plain
`<link rel="stylesheet" href="/X"/>` so the browser fetches the stylesheets
copied verbatim into $out by the flake. All other Trunk directives
(`rel="rust"`, `rel="copy-file"`, `rel="copy-dir"`) and their surrounding
comments are removed since their effects are reproduced separately.

Usage: process-html.py <src> <dst>
"""
import sys
import re

src_path, dst_path = sys.argv[1], sys.argv[2]

with open(src_path) as f:
    html = f.read()

# Rewrite CSS links: <link data-trunk rel="css" href="styles/foo.css"/>
#                 → <link rel="stylesheet" href="/styles/foo.css"/>
def _rewrite_css(match):
    indent = match.group(1)
    href = match.group(2)
    return f'{indent}<link rel="stylesheet" href="/{href}"/>\n'

html = re.sub(
    r'([ \t]*)<link data-trunk rel="css" href="([^"]+)"\s*/>\s*\n',
    _rewrite_css,
    html,
)

# Drop the remaining Trunk directives (rust / copy-file / copy-dir) and
# the comments that explain them.
html = re.sub(r'[ \t]*<!-- Trunk replaces[^\n]*\n', '', html)
html = re.sub(r'[ \t]*<!-- Static binary[^\n]*\n', '', html)
html = re.sub(r'[ \t]*<!-- Nebulae runtime[^\n]*\n', '', html)
html = re.sub(r'[ \t]*<link data-trunk[^\n]*\n', '', html)

# Inject WASM init script before </head>, matching the pattern Trunk produces
init_script = (
    '    <script type="module">\n'
    "import init, * as bindings from '/junos-web.js';\n"
    "const wasm = await init({ module_or_path: '/junos-web_bg.wasm' });\n"
    'window.wasmBindings = bindings;\n'
    'dispatchEvent(new CustomEvent("TrunkApplicationStarted", {detail: {wasm}}));\n'
    '</script>\n'
)
html = html.replace('</head>', init_script + '</head>')

with open(dst_path, 'w') as f:
    f.write(html)
