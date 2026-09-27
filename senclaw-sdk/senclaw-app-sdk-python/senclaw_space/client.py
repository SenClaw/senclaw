"""Talking to the SenClaw daemon from a Python Space App.

Everything an app is allowed to do beyond its own process goes through the
daemon over loopback: storing settings, querying its own SQLite database, and —
the important one — asking a model anything. An app never holds a provider API
key; it calls the bridge and the daemon uses the user's configured provider.

Standard library only, on purpose. A Space App with no third-party dependencies
needs no ``pip install`` at all, so the daemon's prepare step is a no-op and the
app starts in the time it takes Python to boot. Add ``requests`` if you want it,
but you do not need it for any of this.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Iterable


#: Env var carrying this app's access token into its process, set by the daemon
#: on every launch.
#:
#: The daemon mints one token per installed app. Presenting it on
#: ``/api/space/apps/<id>/…`` is what tells the daemon *which* app is calling: a
#: token is bound to one app id, and using it against another is refused.
#: Without it, any local process that knows an app's id — which is public —
#: could read that app's settings, query its database and drive its AI bridge.
ENV_APP_TOKEN = "SENCLAW_TOKEN_ACCESS_APP"

#: Env var carrying the Space-App API contract version.
ENV_API_VERSION = "SENCLAW_API_VERSION"

#: Header the token travels in.
HEADER_APP_TOKEN = "X-SenClaw-App-Token"

#: Header the contract version travels in, both directions.
HEADER_API_VERSION = "X-SenClaw-Api-Version"

#: The Space-App API contract this SDK is written against. Sent on every call;
#: a daemon serving an older contract answers 426 rather than half-answering.
API_VERSION = 2


class SenclawError(RuntimeError):
    """The daemon answered, and the answer was no."""

    def __init__(self, message: str, status: int | None = None) -> None:
        super().__init__(message)
        self.status = status


@dataclass(frozen=True)
class LlmUsage:
    """Provider-reported token usage for one ``llm.request``.

    ``input_tokens`` is the TOTAL billed input — cache tokens included, not on
    top of. The two cache fields break it down for providers that report them
    (Anthropic); adding them to ``input_tokens`` double-counts.
    """

    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0


@dataclass(frozen=True)
class LlmReply:
    """The full reply shape from :meth:`SenclawSpace.llm_detailed`."""

    text: str
    model: str
    #: ``"length"`` (hit the token cap), ``"stop"``, or ``""`` when unreported.
    finish: str
    #: ``None`` when the provider reported no usage — unknown, not zero.
    usage: LlmUsage | None = None


@dataclass(frozen=True)
class KnowledgeHit:
    """One hit from :meth:`SenclawSpace.knowledge_search`."""

    name: str
    summary: str
    score: float


@dataclass(frozen=True)
class ModelInfo:
    """One LLM configured in the daemon."""

    id: str
    model_name: str | None = None
    provider: str | None = None


def app_id_from_env(default: str | None = None) -> str:
    """The id the daemon launched this app under.

    Set as ``SENCLAW_SPACE_APP_ID`` on every launch. Falling back to a
    hard-coded default is fine for local development and wrong in production —
    the id decides which config rows and which database the app gets.
    """
    value = os.environ.get("SENCLAW_SPACE_APP_ID") or default
    if not value:
        raise SenclawError(
            "SENCLAW_SPACE_APP_ID is not set. Run the app through SenClaw, or "
            "pass app_id= explicitly when constructing SenclawSpace."
        )
    return value


def bind_host() -> str:
    """The interface an app may listen on.

    Loopback unless the operator explicitly opted out. A Space App
    authenticates nothing of its own — the daemon reaches it over 127.0.0.1 and
    its UI is same-origin — so binding ``0.0.0.0`` hands the whole REST + MCP
    surface to anyone on the network.
    """
    return os.environ.get("SENCLAW_BIND_HOST", "127.0.0.1")


def app_token_from_env(default: str = "") -> str:
    """The access token the daemon issued this app, or ``""`` outside SenClaw.

    Empty is not an error: a daemon on the default ``SENCLAW_APP_TOKEN_MODE=off``
    serves tokenless calls exactly as it always did. Under ``strict`` they are
    refused — which is the point.
    """
    return os.environ.get(ENV_APP_TOKEN, default).strip()


def api_version_from_env() -> int:
    """The contract version the daemon launched this app under."""
    raw = os.environ.get(ENV_API_VERSION, "").strip()
    if raw.isdigit() and int(raw) > 0:
        return int(raw)
    return API_VERSION


def port(default: int = 0) -> int:
    """The port the daemon assigned, from ``PORT``."""
    raw = os.environ.get("PORT", "").strip()
    if raw.isdigit():
        return int(raw)
    if default:
        return default
    raise SenclawError("PORT is not set and no default was given")


class SenclawSpace:
    """A client for one Space App's slice of the daemon API."""

    def __init__(
        self,
        app_id: str | None = None,
        base_url: str | None = None,
        timeout: float = 60.0,
        app_token: str | None = None,
        api_version: int | None = None,
    ) -> None:
        self.app_id = app_id or app_id_from_env()
        self.base_url = (
            base_url or os.environ.get("SENCLAW_BASE_URL") or "http://127.0.0.1:18788"
        ).rstrip("/")
        self.timeout = timeout
        #: Sent on every call. Pass it explicitly when running the app by hand
        #: against a live daemon — Plugins → Space Apps shows the token, as does
        #: ``GET /api/space/apps/<id>/token``.
        self.app_token = (app_token or app_token_from_env()).strip()
        self.api_version = api_version or api_version_from_env()

    # -- plumbing ---------------------------------------------------------

    def _request(
        self,
        method: str,
        path: str,
        body: Any = None,
        *,
        timeout: float | None = None,
    ) -> Any:
        url = f"{self.base_url}{path}"
        data = None
        headers = {"Accept": "application/json"}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        # Who is calling, and under which contract. An empty token is omitted
        # rather than sent blank: the daemon would try to resolve "" and refuse
        # a call that its default mode would have served.
        if self.app_token:
            headers[HEADER_APP_TOKEN] = self.app_token
        if self.api_version:
            headers[HEADER_API_VERSION] = str(self.api_version)
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=timeout or self.timeout) as resp:
                raw = resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            raw = e.read().decode("utf-8", "replace")
            detail = raw
            try:
                parsed = json.loads(raw)
                detail = parsed.get("error") or parsed.get("message") or raw
            except Exception:
                pass
            raise SenclawError(f"{method} {path} → HTTP {e.code}: {detail}", e.code) from None
        except urllib.error.URLError as e:
            raise SenclawError(f"{method} {path} → {e.reason}") from None
        if not raw:
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            return raw

    def _app_path(self, suffix: str) -> str:
        return f"/api/space/apps/{urllib.parse.quote(self.app_id)}{suffix}"

    # -- config KV --------------------------------------------------------

    def get_config(self, key: str, default: Any = None) -> Any:
        """One stored setting, or ``default`` when it has never been set.

        Shared with the app's own UI, which reads and writes the same keys — so
        this is where settings belong, not in a file inside the app directory
        that an update would overwrite.
        """
        try:
            payload = self._request("GET", self._app_path(f"/config/{urllib.parse.quote(key)}"))
        except SenclawError as e:
            if e.status == 404:
                return default
            raise
        if payload is None:
            return default
        return payload.get("value", default) if isinstance(payload, dict) else payload

    def set_config(self, key: str, value: Any) -> Any:
        payload = self._request(
            "PUT", self._app_path(f"/config/{urllib.parse.quote(key)}"), {"value": value}
        )
        return payload.get("value") if isinstance(payload, dict) else payload

    def delete_config(self, key: str) -> None:
        self._request("DELETE", self._app_path(f"/config/{urllib.parse.quote(key)}"))

    def list_config(self) -> list[dict[str, Any]]:
        payload = self._request("GET", self._app_path("/config")) or {}
        return payload.get("items", []) if isinstance(payload, dict) else []

    # -- sqlite -----------------------------------------------------------

    def sqlite(self, sql: str, params: Iterable[Any] = ()) -> dict[str, Any]:
        """Run one statement against this app's own database.

        Parameterised: pass values in ``params``, never by formatting them into
        ``sql``. The daemon is the only thing that opens this file, so an
        injection here is an injection into every other app's neighbour.
        """
        return self._request(
            "POST", self._app_path("/sqlite/query"), {"sql": sql, "params": list(params)}
        ) or {}

    # -- the AI bridge ----------------------------------------------------

    def bridge(self, action: str, payload: dict[str, Any], timeout: float | None = None) -> Any:
        """Call one of the daemon's bridge actions.

        The generic form. Prefer the named wrappers below, which document the
        traps in each.

        The wire field is ``action``. The daemon's request struct requires it
        and defines no alias, so any other spelling is a 422 before a line of
        handler code runs — which reads as "the bridge is down" rather than
        "you sent the wrong key".
        """
        result = self._request(
            "POST",
            self._app_path("/bridge"),
            {"action": action, "payload": payload},
            timeout=timeout,
        )
        # A failed bridge action comes back as **HTTP 200** carrying
        # ``{"status": "error", "message": ...}`` — the transport worked, the
        # action did not. Checking only the HTTP code turns a dead provider
        # into an empty string, which reads downstream as "the model had
        # nothing to say".
        if isinstance(result, dict) and "status" in result and result.get("status") != "ok":
            if result.get("status") == "pending":
                raise SenclawError(f"bridge action {action!r} is not enabled in this daemon")
            raise SenclawError(str(result.get("message") or f"bridge action {action!r} failed"))
        return result

    def capabilities(self) -> list[str]:
        """What this daemon's bridge actually supports, asked of the daemon."""
        result = self.bridge("capabilities", {})
        if isinstance(result, dict):
            caps = result.get("capabilities")
            if isinstance(caps, list):
                return [str(c) for c in caps]
        return []

    def llm(
        self,
        prompt: str,
        system: str | None = None,
        max_tokens: int = 4000,
        profile: str | None = None,
        timeout: float = 300.0,
    ) -> str:
        """One model call, through the user's configured provider.

        Only ``system``, ``prompt``, ``maxTokens`` and ``profile`` are read —
        there is no temperature knob, and passing one is silently ignored
        rather than honoured.

        Watch ``max_tokens``: a reply that hits the ceiling comes back
        truncated with ``finish == "length"``, which reads as a model that gave
        a short answer rather than as an error. Chunk long work; do not raise
        the ceiling and hope.
        """
        payload: dict[str, Any] = {"prompt": prompt, "maxTokens": max_tokens}
        if system:
            payload["system"] = system
        if profile:
            payload["profile"] = profile
        result = self.bridge("llm.request", payload, timeout=timeout)
        if isinstance(result, dict):
            if result.get("finish") == "length":
                raise SenclawError(
                    "the model hit maxTokens and the reply is truncated — "
                    "split the work into smaller chunks rather than raising the ceiling"
                )
            return result.get("text") or result.get("content") or ""
        return str(result or "")

    def agent(
        self,
        prompt: str,
        tools: list[str] | None = None,
        timeout: float = 900.0,
    ) -> Any:
        """Run a full agent turn — tools, multiple steps, the lot.

        Slower and far more capable than :meth:`llm`. Use it when the work
        needs the agent's tools; use ``llm`` when it needs a paragraph of text.
        """
        payload: dict[str, Any] = {"prompt": prompt}
        if tools:
            payload["tools"] = tools
        return self.bridge("agent.run", payload, timeout=timeout)

    def llm_detailed(
        self,
        prompt: str,
        system: str | None = None,
        max_tokens: int = 4000,
        profile: str | None = None,
        timeout: float = 300.0,
    ) -> LlmReply:
        """The same call as :meth:`llm`, returning everything the provider said.

        Use this when you want to *handle* a truncated reply instead of having
        it raised at you (``finish == "length"`` means the cap was hit), or when
        you need real token counts. ``usage`` is ``None`` when the provider
        reported none — some local models do — which means unknown, not zero.
        """
        payload: dict[str, Any] = {"prompt": prompt, "maxTokens": max_tokens}
        if system:
            payload["system"] = system
        if profile:
            payload["profile"] = profile
        result = self.bridge("llm.request", payload, timeout=timeout)
        if not isinstance(result, dict):
            return LlmReply(text=str(result or ""), model="", finish="", usage=None)
        raw = result.get("usage")
        usage = None
        if isinstance(raw, dict):
            def n(key: str) -> int:
                v = raw.get(key)
                return int(v) if isinstance(v, (int, float)) else 0

            usage = LlmUsage(
                input_tokens=n("inputTokens"),
                output_tokens=n("outputTokens"),
                cache_read_tokens=n("cacheReadTokens"),
                cache_creation_tokens=n("cacheCreationTokens"),
            )
        return LlmReply(
            text=result.get("text") or result.get("content") or "",
            model=result.get("model") or "",
            finish=result.get("finish") or "",
            usage=usage,
        )

    # -- knowledge --------------------------------------------------------
    #
    # Each *space* is an independent memory partition. Omitting ``space`` uses
    # the app's own private one, named after the app id — so an app that never
    # passes a space can neither read nor pollute anybody else's memory.

    def knowledge_save(
        self,
        text: str,
        space: str | None = None,
        source: str | None = None,
        tags: list[str] | None = None,
    ) -> None:
        """Save one memory into a knowledge space."""
        payload: dict[str, Any] = {"text": text}
        if space:
            payload["space"] = space
        if source:
            payload["source"] = source
        if tags:
            payload["tags"] = tags
        self.bridge("knowledge.save", payload)

    def knowledge_search(
        self, query: str, space: str | None = None, limit: int = 10
    ) -> list[KnowledgeHit]:
        """Scoped search over one knowledge space — raw hits, no synthesis."""
        payload: dict[str, Any] = {"query": query, "limit": limit}
        if space:
            payload["space"] = space
        result = self.bridge("knowledge.search", payload)
        hits = result.get("hits") if isinstance(result, dict) else None
        if not isinstance(hits, list):
            return []
        out: list[KnowledgeHit] = []
        for h in hits:
            if not isinstance(h, dict):
                continue
            score = h.get("score")
            out.append(
                KnowledgeHit(
                    name=str(h.get("name") or ""),
                    summary=str(h.get("summary") or ""),
                    score=float(score) if isinstance(score, (int, float)) else 0.0,
                )
            )
        return out

    def knowledge_recall(
        self,
        query: str,
        space: str | None = None,
        limit: int | None = None,
        hops: int | None = None,
    ) -> str:
        """Scoped recall *with* LLM synthesis — one answer, not a hit list.

        Returns ``""`` when the space holds nothing relevant. That is a real
        answer, not an error.
        """
        payload: dict[str, Any] = {"query": query}
        if space:
            payload["space"] = space
        if limit is not None:
            payload["limit"] = limit
        if hops is not None:
            payload["hops"] = hops
        result = self.bridge("knowledge.recall", payload)
        if isinstance(result, dict):
            return str(result.get("answer") or "")
        return ""

    # -- accounting & models ----------------------------------------------

    def usage_report(
        self,
        model: str,
        provider: str,
        input_tokens: int,
        output_tokens: int,
        latency_ms: int = 0,
        estimated: bool = False,
    ) -> None:
        """Report tokens for a call the app made **directly** to a provider.

        Only for apps holding their own API key and bypassing :meth:`llm` — it
        keeps the daemon's accounting whole. Fire-and-forget: a failure here
        must never take down the work it describes, so errors are swallowed.
        Pass ``estimated=True`` when the numbers are chars/4 guesses.
        """
        try:
            self.bridge(
                "usage.report",
                {
                    "model": model,
                    "provider": provider,
                    "inputTokens": input_tokens,
                    "outputTokens": output_tokens,
                    "latencyMs": latency_ms,
                    "estimated": estimated,
                },
            )
        except Exception:  # noqa: BLE001 - accounting never fails the caller
            pass

    def list_models(self) -> tuple[str | None, list[ModelInfo]]:
        """The daemon's configured LLMs, and which one is active."""
        v = self.core("/api/llm-config")
        if not isinstance(v, dict):
            return None, []
        active = v.get("activeId")
        configs = v.get("configs")
        models: list[ModelInfo] = []
        if isinstance(configs, list):
            for c in configs:
                if not isinstance(c, dict) or not isinstance(c.get("id"), str):
                    continue
                models.append(
                    ModelInfo(
                        id=c["id"],
                        model_name=c.get("modelName"),
                        provider=c.get("provider") or c.get("adapt"),
                    )
                )
        return (active if isinstance(active, str) else None), models

    def set_active_model(self, model_id: str) -> None:
        """Switch the daemon's active main model.

        **Global** — the agent and every other app share it. An app that wants
        its own model should pass ``profile`` to :meth:`llm` rather than moving
        everyone else's cheese.
        """
        self.core("/api/llm-config/active", method="POST", body={"id": model_id})

    # -- everything else --------------------------------------------------

    def register_mcp(self, registration: dict[str, Any]) -> Any:
        """Register an MCP server with the daemon on this app's behalf.

        ``registration`` takes ``transport`` (``stdio`` | ``sse`` | ``http``)
        plus the fields that transport needs — ``url``, or ``command``/``args``
        /``env`` — and optionally ``name``, ``description``, ``use_tools``,
        ``enabled``.
        """
        return self._request("POST", self._app_path("/mcp/register"), registration)

    def core(self, path: str, method: str = "GET", body: Any = None) -> Any:
        """Any other daemon endpoint, e.g. ``core("/api/wiki/list")``."""
        if not path.startswith("/"):
            path = "/" + path
        return self._request(method, path, body)
