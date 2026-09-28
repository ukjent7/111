// The gateway serves this page in two places: the app's own desktop window
// (the shell loads it with ?shell=1) and a plain browser tab at the gateway's
// address — that one is on its own, no window of the app's around it
window.bootPrefs = { web: !new URLSearchParams(location.search).has("shell") };
