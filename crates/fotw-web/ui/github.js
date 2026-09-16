// The per-meeting GitHub sync line (issue #112).
//
// There is deliberately no "Push to GitHub" button here. The worker owns every
// push, so a button on a synced meeting had nothing to do and invited a second
// commit of a meeting already in the repository. What a meeting shows instead is
// the state the daemon reports: never synced, synced and when, changed since
// that sync, or failed and why. Only the failed state offers a control, because
// only a failed meeting has something a person can usefully ask for.
//
// Its own file rather than another section of app.js for the reason documents.js
// is its own file: app.js reaches for `document.getElementById` at load and
// calls `main()` at the bottom, so it cannot be loaded into a test context,
// while this can — `crates/fotw-web/tests/ui/github.cjs` drives every state
// through the same DOM the browser gives it.
//
// ING-11 holds here as everywhere: `text()` assigns textContent, and the
// markup-assigning DOM properties appear nowhere in this file. The reason is
// sharper than usual for the failed state — its message is `gh`'s own stderr,
// which is GitHub's words about a repository, not ours.

// Nothing here is rendered yet: the states below are what the tests in
// crates/fotw-web/tests/ui/github.cjs pin, and the implementation follows.
function mountGithubSync(detail, host) {}
