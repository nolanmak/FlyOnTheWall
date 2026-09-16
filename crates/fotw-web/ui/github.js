// The per-meeting GitHub sync line (issue #112).
//
// There is deliberately no "Push to GitHub" button here. The worker owns every
// push, so a button on a synced meeting had nothing to do and invited a second
// commit of a meeting already in the repository. What a meeting shows instead is
// the state the daemon reports: never synced, synced and when, changed since
// that sync, or failed and why. Only the failed state offers a control, because
// only a failed meeting has something a person can usefully ask for — and even
// then the worker is already going to retry on its own, so the control is a
// shortcut rather than the way back.
//
// Its own file rather than another section of app.js for the reason documents.js
// is its own file: app.js reaches for `document.getElementById` at load and
// calls `main()` at the bottom, so it cannot be loaded into a test context,
// while this can — `crates/fotw-web/tests/ui/github.cjs` drives every state
// through the same DOM the browser gives it.
//
// It takes `text`, `clear`, `api` and `say` from app.js, exactly as documents.js
// does. The shell loads this file first so that `ghExplain` below is defined
// before app.js's own settings form calls it.
//
// ING-11 holds here as everywhere: `text()` assigns textContent, and the
// markup-assigning DOM properties appear nowhere in this file. The reason is
// sharper than usual for the failed state — its message is `gh`'s own stderr,
// which is GitHub's words about a repository and not ours.

// The stable machine codes from the API, spelled for a person. Anything not
// listed renders verbatim — GitHub's own error text beats a shrug.
const GH_ERRORS = {
  gh_missing: "The gh CLI is not installed. brew install gh, then try again.",
  gh_not_authenticated: "gh has no login. Run gh auth login in a terminal, then try again.",
  repo_not_found: "That repository is not reachable with your gh login. Check the name and your access.",
  github_export_disabled: "GitHub export is switched off. Enable it in the GitHub export section first.",
  repo_is_public: "Nothing was pushed: that repository is public, or GitHub did not confirm it is private. Choose a private repository, or tick 'push to a public repository anyway' and save, in the GitHub export section.",
};

function ghExplain(code) {
  return GH_ERRORS[code] || code;
}

// The local clock, as the rest of the dashboard prints times. A sync is an event
// on this machine, so it is read in this machine's timezone.
function syncWhen(ms) {
  if (!ms) return "";
  return new Date(ms).toLocaleString();
}

// One sentence per state, and never more than one on screen at a time: the
// whole complaint in #112 was that "already in the repository" and "never
// pushed" were indistinguishable.
function syncLine(status) {
  if (status.state === "never") {
    return "Not synced to GitHub yet.";
  }
  if (status.state === "synced") {
    return "Synced to " + status.repo + " on " + syncWhen(status.pushed_at_ms) + ".";
  }
  if (status.state === "changed") {
    return (
      "Changed since it was synced to " +
      status.repo +
      " on " +
      syncWhen(status.pushed_at_ms) +
      ". The next pass will sync it again."
    );
  }
  // Failed. The reason is `gh`'s, so it is shown rather than summarised.
  let line = "This meeting did not sync: " + (status.error || "no reason was recorded") + ".";
  // Only when the daemon actually said it will try again. Promising an
  // automatic retry that is not scheduled would be the same kind of lie as the
  // button that had nothing to do.
  if (status.retry_at_ms) line += " The daemon will try again on its own.";
  return line;
}

function mountGithubSync(detail, host) {
  // Only a finished meeting can have been exported, and asking about one that
  // cannot would be a request per row of a live pane.
  if (detail.meeting.state !== "ready") return;
  const id = detail.meeting.id;
  let panel = null;
  let busy = false;

  // Created on the first state worth showing rather than up front, so a build
  // with no GitHub export — and a switched-off target — adds nothing to the
  // meeting at all.
  function surface() {
    if (!panel) {
      panel = document.createElement("section");
      panel.className = "gh-sync-panel";
      host.appendChild(panel);
    }
    clear(panel);
    return panel;
  }

  function render(status) {
    if (!status || status.state === "off") {
      if (panel) {
        panel.remove();
        panel = null;
      }
      return;
    }
    const root = surface();
    root.appendChild(text("p", syncLine(status), "gh-sync"));
    if (status.state !== "failed") return;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "gh-retry";
    button.textContent = "Retry";
    button.disabled = busy;
    button.addEventListener("click", retry);
    root.appendChild(button);
  }

  async function load() {
    try {
      render(await api("/api/meetings/" + encodeURIComponent(id) + "/github-sync"));
    } catch (e) {
      // Not "the request failed" — this build has no GitHub export at all, and
      // the same 404 convention the recorder and the settings form use applies.
      render(null);
    }
  }

  // The retry is a push of this meeting, now. The outcome is said out loud and
  // then the state is re-read, so what stays on screen is what the daemon
  // reports rather than what this handler hoped for.
  async function retry() {
    if (busy) return;
    busy = true;
    // `githubSettings` belongs to app.js, and this module is loaded before it
    // and exercised on its own by tests/ui/github.cjs — so it is reached the
    // way documents.js reaches it, through a `typeof` guard rather than a bare
    // read that would throw a ReferenceError.
    const target =
      typeof githubSettings !== "undefined" && githubSettings ? githubSettings.repo : "GitHub";
    say("Retrying the push to " + target + "…");
    try {
      const body = await api("/api/meetings/" + encodeURIComponent(id) + "/github-push", {
        method: "POST",
      });
      if (body.error) say(ghExplain(body.error));
      else say("Synced: " + body.receipt.repo + "/" + body.receipt.path);
    } catch (e) {
      say("Could not reach the daemon to retry that push.");
    }
    busy = false;
    await load();
  }

  load();
}
