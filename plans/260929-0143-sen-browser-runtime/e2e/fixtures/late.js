// The script of an app shell: what the page shows arrives `ms` after it ran.
const ms = Number(new URL(document.currentScript.src).searchParams.get('ms') || 0);
setTimeout(() => window.renderClips(), ms);
