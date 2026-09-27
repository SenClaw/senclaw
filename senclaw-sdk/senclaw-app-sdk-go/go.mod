module github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go

// The standard library and nothing else, on purpose: a Space App with no
// third-party dependencies has no module download step before its first build,
// and `go build` on an air-gapped machine still works.
go 1.21
