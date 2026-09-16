// The per-meeting GitHub sync line (issue #112).
//
// There is deliberately no "Push to GitHub" button here. The worker owns every
// push, so a button on a synced meeting had nothing to do and invited a second
// commit of a meeting already in the repository. What a meeting shows instead is
// the state the daemon reports: never synced, synced and when, changed since
// that sync, or failed and why.
//
// A control appears exactly where the worker will *not* do the job. The daemon
// bounds what its automatic pass owes by the mode, the auto stamp and the
// whole-library switch, and says so in `scheduled`: a meeting in manual mode, or
// one recorded before automatic pushes were switched on, is reached by no pass
// at all and would otherwise sit there forever with nothing on screen able to
// change it. So that meeting gets **Sync now**, a failed meeting keeps
// **Retry**, and a meeting the worker is going to take gets no control and says
// that it is coming — which is the distinction the pane could not express while
// every state read the same.
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
  // Neither of these two opens with its own verdict. Both are composed into a
  // sentence that already has one — "No meeting can be pushed right now: …" —
  // and a second verdict there read as two stapled together.
  github_export_disabled: "GitHub export is switched off. Enable it in the GitHub export section first.",
  repo_is_public: "that repository is public, or GitHub did not confirm it is private. Choose a private repository, or tick 'push to a public repository anyway' and save, in the GitHub export section.",
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

// Will an automatic pass push this meeting on its own?
//
// An absent field means yes: a daemon that answers without it is one whose
// worker owed every meeting it could see. That is also the safe way round — the
// answer that draws no control — because a control offered beside a worker that
// is already about to push is the duplicate commit this issue exists to stop.
function syncScheduled(status) {
  return status.scheduled !== false;
}

// Is the repository's copy of this meeting missing or out of date?
function syncOwed(status) {
  return status.state === "never" || status.state === "changed";
}

// One sentence per state, and never more than one on screen at a time: the
// whole complaint in #112 was that "already in the repository" and "never
// pushed" were indistinguishable.
function syncLine(status) {
  if (status.state === "synced") {
    return "Synced to " + status.repo + " on " + syncWhen(status.pushed_at_ms) + ".";
  }
  if (syncOwed(status)) {
    const line =
      status.state === "never"
        ? "Not synced to GitHub yet."
        : "Changed since it was synced to " +
          status.repo +
          " on " +
          syncWhen(status.pushed_at_ms) +
          ".";
    // A refusal that answers for every meeting — no gh, no login, the
    // repository gone or public. Nothing is parked for it and nothing retries
    // it on a schedule, so this says what is wrong rather than promising a pass
    // that will fail the same way.
    if (status.blocked) {
      let stalled = line + " No meeting can be pushed right now: " + ghExplain(status.blocked);
      if (status.blocked_at_ms) stalled += " Last tried " + syncWhen(status.blocked_at_ms) + ".";
      return stalled;
    }
    if (syncScheduled(status)) {
      return line + (status.state === "never" ? " The next pass will sync it." : " The next pass will sync it again.");
    }
    return (
      line +
      " No automatic pass covers this meeting, so " +
      (status.state === "never"
        ? "use Sync now to send it."
        : "use Sync now to send the new version.")
    );
  }
  // Failed. The reason is `gh`'s, so it is shown rather than summarised.
  let line = "This meeting did not sync: " + (status.error || "no reason was recorded") + ".";
  // An earlier push is still in the repository. Dropping this read as a meeting
  // that had never synced at all, which is a different and worse fact.
  if (status.pushed_at_ms) line += " It was last synced on " + syncWhen(status.pushed_at_ms) + ".";
  // Only when the daemon actually said it will try again — it sends a retry
  // time for the meetings its pass covers and for no others. Promising an
  // automatic retry that is not scheduled would be the same kind of lie as the
  // button that had nothing to do.
  if (status.retry_at_ms) line += " The daemon will try again on its own.";
  else line += " No automatic pass covers this meeting, so it will not be retried on its own.";
  return line;
}

// The control this state offers, or null for the states that need none.
//
// The accessible name says more than the visible label — out of context "Retry"
// says nothing about what is being retried, and the pane is one of several in a
// meeting — but it *begins* with that label. WCAG 2.5.3 (Label in Name): a
// speech-input user saying "click Sync now" has to be able to activate the one
// control a meeting outside every pass has.
function syncControl(status) {
  if (status.state === "failed") {
    return {
      label: "Retry",
      className: "gh-retry",
      name: "Retry: push this meeting to GitHub again",
    };
  }
  if (syncOwed(status) && !syncScheduled(status)) {
    return {
      label: "Sync now",
      className: "gh-sync-now",
      name: "Sync now: push this meeting to GitHub",
    };
  }
  return null;
}

function mountGithubSync(detail, host) {
  // Only a finished meeting can have been exported, and asking about one that
  // cannot would be a request per row of a live pane.
  if (detail.meeting.state !== "ready") return;
  const id = detail.meeting.id;

  // Owned before the first await, and appended to the host this call was given.
  // `#detail` is a single node that renderDetail clears and reuses, so a panel
  // created after the GET resolved was appended to whatever pane was on screen
  // by then — switching meetings mid-flight left two sync lines in it. Hidden
  // until there is a state worth showing, so a build with no GitHub export, and
  // a switched-off target, still add nothing to the meeting.
  const panel = document.createElement("section");
  panel.className = "gh-sync-panel";
  panel.setAttribute("aria-label", "GitHub sync");
  panel.hidden = true;
  host.appendChild(panel);

  let busy = false;
  // The state on screen, so the control can be redrawn without re-reading it.
  let current = null;

  // Draw into this pane's own panel, unless the reader has moved on: a load
  // whose panel left the document has nothing to say about the meeting that
  // replaced it.
  function paint(line, control) {
    if (!panel.isConnected) return;
    clear(panel);
    panel.hidden = false;
    panel.appendChild(text("p", line, "gh-sync"));
    if (!control) return;
    const button = document.createElement("button");
    button.type = "button";
    button.className = control.className;
    button.textContent = control.label;
    button.setAttribute("aria-label", control.name);
    button.disabled = busy;
    button.addEventListener("click", sync);
    panel.appendChild(button);
  }

  function render(status) {
    current = status;
    if (!status || status.state === "off") {
      panel.remove();
      return;
    }
    paint(syncLine(status), syncControl(status));
  }

  async function load() {
    try {
      render(await api("/api/meetings/" + encodeURIComponent(id) + "/github-sync"));
    } catch (e) {
      // A 404 is this build having no GitHub export at all — the same
      // convention the recorder and the settings form use — and the panel goes
      // with it. Anything else is a problem worth reporting: rendering a 500,
      // an expired token or a daemon mid-restart as "there is no such feature"
      // took the meeting's state off screen and said nothing was wrong.
      if (e && e.status === 404) {
        render(null);
        return;
      }
      current = null;
      paint("This meeting's GitHub sync state could not be read. Reopen the meeting to try again.", null);
    }
  }

  // The control is a push of this meeting, now. The outcome is said out loud and
  // then the state is re-read, so what stays on screen is what the daemon
  // reports rather than what this handler hoped for.
  async function sync() {
    if (busy) return;
    busy = true;
    // Redrawn immediately. The disabled state is decided where the button is
    // built, so without this the control stayed live for the whole length of the
    // push and a second click asked for a second commit of the same meeting.
    render(current);
    // The repository the state is about. Reading it from the settings form
    // instead named whatever is configured *now*, which is not necessarily the
    // repository this meeting failed against.
    const target = (current && current.repo) || "GitHub";
    say("Syncing this meeting to " + target + "…");
    try {
      const body = await api("/api/meetings/" + encodeURIComponent(id) + "/github-push", {
        method: "POST",
      });
      if (body.error) say(ghExplain(body.error));
      else say("Synced: " + body.receipt.repo + "/" + body.receipt.path);
    } catch (e) {
      say("Could not reach the daemon to sync that meeting.");
    }
    busy = false;
    await load();
  }

  load();
}
