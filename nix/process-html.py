#!/usr/bin/env python3
"""Strip Trunk directives from index.html and inject the WASM init script.

Usage: process-html.py <src> <dst>
"""
import sys
import re

src_path, dst_path = sys.argv[1], sys.argv[2]

with open(src_path) as f:
    html = f.read()

# Remove Trunk-specific comment and all <link data-trunk ...> directives
html = re.sub(r'[ \t]*<!-- Trunk replaces[^\n]*\n', '', html)
html = re.sub(r'[ \t]*<link data-trunk[^\n]*\n', '', html)
html = re.sub(r'[ \t]*<!-- Static binary[^\n]*\n', '', html)
html = re.sub(r'[ \t]*<!-- Nebulae runtime[^\n]*\n', '', html)

# Inject WASM init script before </head>, matching the pattern Trunk produces
init_script = (
    '    <script type="module">\n'
    "import init, * as bindings from '/rekos-wasm.js';\n"
    "const wasm = await init({ module_or_path: '/rekos-wasm_bg.wasm' });\n"
    'window.wasmBindings = bindings;\n'
    'dispatchEvent(new CustomEvent("TrunkApplicationStarted", {detail: {wasm}}));\n'
    '</script>\n'
)
html = html.replace('</head>', init_script + '</head>')

with open(dst_path, 'w') as f:
    f.write(html)
