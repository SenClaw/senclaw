// Closing a Space App's own API to everything except the SenClaw daemon.
//
// A Space App authenticates nothing of its own. It listens on a loopback port,
// and every process on the machine can reach that port: its REST API, its MCP
// endpoint, its database-backed tools. Binding loopback keeps the LAN out; it
// does nothing about the browser extension, the other Space App, or the script
// that happens to know the port.
//
// The daemon mints one access token per installed app and stamps it on every
// request it forwards — the UI iframe, the app's own `fetch`es, MCP tool calls
// — so an app can require it and become reachable only through the daemon.
//
// This is the *logic*; `Serve(...)` applies it when `requireAppToken` is set.
// Two things are deliberately not refused:
//
//   - No token in the environment. That is a bare `swift run` outside SenClaw,
//     and 401ing every request — including the daemon's health check — would
//     turn "no token issued" into "app permanently down".
//   - Exempt paths. Pass the health path and anything a client dials directly,
//     such as a browser extension's WebSocket. The health check decides whether
//     the app started, and it runs before anything is ever proxied.

import Foundation

/// Constant-time compare — `==` on a secret short-circuits at the first
/// differing byte and leaks the matched prefix length through timing.
public func constantTimeEquals(_ a: String, _ b: String) -> Bool {
    let x = Array(a.utf8)
    let y = Array(b.utf8)
    if x.count != y.count { return false }
    var acc: UInt8 = 0
    for i in 0..<x.count { acc |= x[i] ^ y[i] }
    return acc == 0
}

/// Whether a request may proceed, given the app's token and its exempt paths.
///
/// `presented` is the token the request carried (from the header or
/// `?app_token=`), `token` is the one the daemon issued this app. `skip`
/// matches a path exactly, or by prefix when it ends in `*` (`"/public/*"`).
///
/// A `nil`/empty `token` means no token was issued — the app is not running
/// under SenClaw — and the guard is inert.
public func appTokenAuthorized(path: String, presented: String?, token: String?, skip: [String]) -> Bool {
    guard let token, !token.isEmpty else { return true }
    for pattern in skip where !pattern.isEmpty {
        if pattern.hasSuffix("*") {
            if path.hasPrefix(String(pattern.dropLast())) { return true }
        } else if path == pattern {
            return true
        }
    }
    guard let presented, !presented.isEmpty else { return false }
    return constantTimeEquals(presented, token)
}
