//! Ready-made lines for a startup/shutdown shell script, appended to the body
//! by the editor's "Insert snippet…" picker. Each ends in a newline and never
//! exits early on success, so snippets can follow one another. Addresses and
//! names are placeholders for the user to replace.

pub struct Snippet {
    /// Picks the label (`labels::snippet_label`).
    pub key: &'static str,
    pub body: &'static str,
}

pub const SNIPPETS: &[Snippet] = &[
    Snippet {
        key: "wait",
        body: "# Wait 30 seconds\nsleep 30\n",
    },
    Snippet {
        key: "webhook",
        body: "# Call a web hook (home automation, roof controller…)\n\
               curl -fsS -m 30 -X POST \"http://192.168.1.10/hook/observatory\" -d \"event=startup\"\n",
    },
    Snippet {
        key: "notify",
        body: "# Push a phone notification with ntfy.sh — pick your own topic\n\
               curl -fsS -m 30 -d \"Observatory: $(basename \"$0\") done\" https://ntfy.sh/my-observatory\n",
    },
    Snippet {
        key: "indi",
        body: "# Set an INDI property directly (indi_setprop ships with libindi)\n\
               indi_setprop \"Dome Simulator.DOME_SHUTTER.SHUTTER_OPEN=On\"\n",
    },
    Snippet {
        key: "wait_file",
        body: "# Wait up to 5 minutes for a file (e.g. a roof-open flag), else fail\n\
               for i in $(seq 60); do\n\
               \x20 [ -e /tmp/roof_open ] && break\n\
               \x20 [ \"$i\" = 60 ] && { echo \"timed out waiting for /tmp/roof_open\" >&2; exit 1; }\n\
               \x20 sleep 5\n\
               done\n",
    },
    Snippet {
        key: "log",
        body: "# Append a line to a log file\n\
               echo \"$(date -Is) $(basename \"$0\")\" >> \"$HOME/observatory.log\"\n",
    },
    Snippet {
        key: "ping",
        body: "# Fail unless a host answers (camera PC, power switch…)\n\
               ping -c 1 -W 2 192.168.1.10 > /dev/null\n",
    },
];
