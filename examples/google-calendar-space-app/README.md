# Google Calendar Space App

This is an installable Next.js Space App example. It is not built into the Space UI.

The editable source lives in `app/`. SenClaw should not run the Next.js source
folder directly; install only the static export ZIP generated from `out/`.

## Build

```bash
npm install
npm run build
```

Next.js is configured with `output: 'export'`. The build writes the static app
to `out/`, then copies `senclaw-manifest.json` into that output directory.

## Install

Zip the contents of `out/`, not the source folder:

```bash
cd out
zip -r ../google-calendar-space-app.zip .
```

Or run `npm run pack:zip`.

Install the ZIP from `Space -> Apps -> Cài từ ZIP` or `Settings -> Space Apps`.

## Space Settings

The page loads its settings from the SenClaw Space App config KV store through
`SenclawSpace.getConfig('google-calendar-settings')`.
