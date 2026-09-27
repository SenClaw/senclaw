"""Serving an LLM *from* a Python Space App, so SenClaw routes turns to it.

This is the reverse of the AI bridge in :mod:`senclaw_space.client`. There, an
app asks the daemon for a completion. Here the app *is* the model: it declares
an ``llm`` block in its ``senclaw-manifest.json``, the daemon registers every
model it advertises into the same picker as OpenAI and Anthropic, and agent
turns arrive over HTTP as OpenAI ``chat/completions`` requests.

    from senclaw_space import serve
    from senclaw_space.llm import (
        ChatRequest, ChunkSink, LlmProvider, ModelCard, llm_routes, publish_models,
    )

    class Mlx(LlmProvider):
        def models(self):
            return [ModelCard("gemma-4-e2b-it-4bit", 128_000, 8192, vision=True)]

        def chat(self, req: ChatRequest, sink: ChunkSink) -> None:
            sink.text("hello")

    provider = Mlx()
    publish_models(".", provider.models())   # so a *stopped* app still lists
    serve(
        routes={**llm_routes(provider), ("GET", "/health"): lambda r: {"ok": True}},
        health_path="/health",
    )

And the manifest earns the app its place in the picker::

    "llm": { "autoRegister": true, "path": "/v1", "adapt": "openai",
             "displayName": "MLX" }

Why the SDK owns the wire format and not the provider
-----------------------------------------------------
The provider emits **semantic** events — visible text, reasoning, a tool call —
and this module renders them as OpenAI ``chat.completion`` /
``chat.completion.chunk``. That split is the whole point: the daemon's OpenAI
adapter is a real parser with real expectations (``delta.content``,
``delta.reasoning_content``, indexed ``delta.tool_calls`` whose ``name`` and
``arguments`` *accumulate* across chunks). An app that hand-rolled that JSON
would get a corner of it wrong; an app that implements :class:`LlmProvider`
cannot. It also means the daemon needs no special case for a local model — by
the time bytes reach it they are ordinary OpenAI, which is what lets this reuse
``adapt: "openai"`` instead of adding an adapter.

Buffered, not streamed
----------------------
The Rust SDK streams each event to the client as it is produced. The Python
:func:`senclaw_space.serve` harness is synchronous and **buffered** — a handler
returns one whole :class:`~senclaw_space.server.Response`, with no mid-request
flushing and no client-disconnect signal. So :class:`ChunkSink` here
*accumulates*: the route runs ``provider.chat`` to completion, then renders the
collected chunks into either one ``chat.completion`` object (non-stream) or a
complete SSE body (stream). The wire bytes are identical; only *when* they leave
differs, which the daemon's SSE reader does not observe.
"""

from __future__ import annotations

import itertools
import json
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from .server import Request, Response

#: Where the daemon looks for an app's model list while the app is **stopped**.
#:
#: Relative to the app's own directory. A session app is stopped most of the
#: time — that is its resting state — and a model nobody can see in the picker
#: is a model nobody selects, calls, or ever starts the app for. So the list is
#: cached on disk at startup and read from there when the process is gone.
MODELS_CACHE_PATH = ".senclaw/llm-models.json"


# ---------------------------------------------------------------------------
# What a provider advertises
# ---------------------------------------------------------------------------


@dataclass
class ModelCard:
    """One model this app can serve."""

    #: Wire id. This is what arrives in :attr:`ChatRequest.model`, and what the
    #: user sees in the picker unless ``display_name`` says otherwise.
    id: str
    #: Total context window, in tokens.
    context_length: int
    #: Ceiling on one response, in tokens.
    max_output_tokens: int
    #: **Required, never inferred.** SenClaw decides whether to send image
    #: blocks or fall back to OCR from this field, and the consequences are
    #: asymmetric: a text-only endpoint answers an image block with a hard 400
    #: that fails the entire turn, while OCR merely degrades it. Inference from
    #: the model id cannot be trusted — a local checkpoint is named things like
    #: ``mlx-community/Qwen3.5-2B-OptiQ-4bit``, which matches no vendor pattern,
    #: so a guess lands on ``False`` by accident today and ``True`` by accident
    #: the day someone widens a regex. The app has the model's ``config.json``
    #: open; it knows.
    vision: bool
    #: Human label for the picker. ``None`` means fall back to :attr:`id`.
    display_name: str | None = None
    #: Whether the model can be given tools. ``False`` makes it a chat-only
    #: model in the picker.
    tools: bool = True

    def to_json(self) -> dict[str, Any]:
        # ``display_name`` is emitted even when ``None`` (as JSON ``null``) — the
        # daemon's ``Option<String>`` reads that as absent, and the ``/v1/models``
        # endpoint already sends it that way, so the two shapes stay identical.
        return {
            "id": self.id,
            "display_name": self.display_name,
            "context_length": self.context_length,
            "max_output_tokens": self.max_output_tokens,
            "vision": self.vision,
            "tools": self.tools,
        }

    @staticmethod
    def from_json(d: Any) -> "ModelCard":
        if not isinstance(d, dict):
            raise ValueError("a model card must be a JSON object")
        # `tools` defaults to True when the key is absent — a card written by an
        # older SDK that predates the field must not become chat-only on read.
        tools = d.get("tools", True)
        return ModelCard(
            id=str(d.get("id") or ""),
            context_length=int(d.get("context_length") or 0),
            max_output_tokens=int(d.get("max_output_tokens") or 0),
            vision=bool(d.get("vision", False)),
            display_name=d.get("display_name"),
            tools=True if tools is None else bool(tools),
        )


# ---------------------------------------------------------------------------
# One turn
# ---------------------------------------------------------------------------


@dataclass
class ChatRequest:
    """An incoming turn, in OpenAI ``chat/completions`` shape.

    The modelled fields are the ones every provider needs. :attr:`raw` carries
    the whole body besides, because SenClaw sends more than this names —
    HF-style ``tools``, ``stream_options``, provider-specific extras — and a
    provider that understands one of them should not have to fork the SDK.
    """

    #: Which :attr:`ModelCard.id` this turn is for.
    model: str
    #: OpenAI-shaped messages, untouched. Passed through as a raw list rather
    #: than a typed structure: ``content`` is a string on some turns and an
    #: array of parts on others (that is how images arrive), and a lossy
    #: re-encoding here would drop exactly the parts a vision model needs.
    messages: list = field(default_factory=list)
    #: Tool definitions, or empty. OpenAI function shape.
    tools: list = field(default_factory=list)
    #: Did the caller ask for SSE? :func:`llm_routes` handles both, so a
    #: provider normally ignores this.
    stream: bool = False
    #: Output ceiling for this turn, when the caller set one.
    max_tokens: int | None = None
    #: Sampling temperature, when the caller set one.
    temperature: float | None = None
    #: The complete request body.
    raw: Any = None

    @staticmethod
    def from_body(body: Any) -> "ChatRequest":
        """Parse one request body, or raise :class:`ValueError`.

        A missing model or empty message list is refused rather than run — the
        daemon would otherwise get a 200 with an empty answer and read it as a
        model that had nothing to say.
        """
        if not isinstance(body, dict):
            raise ValueError("request body must be a JSON object")
        model = body.get("model")
        if not isinstance(model, str) or not model:
            raise ValueError("`model` is required")
        messages = body.get("messages")
        if not isinstance(messages, list):
            raise ValueError("`messages` must be an array")
        if not messages:
            raise ValueError("`messages` must not be empty")
        tools = body.get("tools")
        # `max_completion_tokens` is the current spelling; `max_tokens` is what
        # older clients (and SenClaw) still send. The newer wins when a client
        # sends both.
        ceiling = _as_int(body.get("max_completion_tokens"))
        if ceiling is None:
            ceiling = _as_int(body.get("max_tokens"))
        return ChatRequest(
            model=model,
            messages=messages,
            tools=tools if isinstance(tools, list) else [],
            stream=bool(body.get("stream", False)),
            max_tokens=ceiling,
            temperature=_as_float(body.get("temperature")),
            raw=body,
        )


# ---------------------------------------------------------------------------
# Generation events
# ---------------------------------------------------------------------------


class Chunk:
    """One semantic event from a running generation. See the subclasses."""


@dataclass(frozen=True)
class Text(Chunk):
    """Visible assistant text, already stripped of any chat-template markers."""

    text: str


@dataclass(frozen=True)
class Reasoning(Chunk):
    """Chain-of-thought, shown separately by SenClaw and echoed back on the
    next request as ``reasoning_content``."""

    text: str


@dataclass(frozen=True)
class ToolCall(Chunk):
    """A completed tool call. Emit it whole: the SDK renders the accumulating
    ``delta.tool_calls`` shape the OpenAI wire requires, so a provider never
    streams partial JSON arguments and hopes they reassemble."""

    id: str
    name: str
    arguments: str


@dataclass(frozen=True)
class Usage(Chunk):
    """Token counts for this turn. Emit at most once, at the end. SenClaw reads
    it into its usage tracking; omitting it costs only the statistics."""

    prompt_tokens: int
    completion_tokens: int


class ChunkSink:
    """The handle a provider writes generation events to.

    Unlike the Rust SDK's channel, this one *collects*: the Python
    :func:`senclaw_space.serve` harness is buffered, so the route accumulates
    every chunk here and renders them after :meth:`LlmProvider.chat` returns.
    """

    def __init__(self) -> None:
        #: Every chunk sent so far, in order. The route reads this after
        #: ``chat`` returns.
        self.chunks: list[Chunk] = []

    def send(self, chunk: Chunk) -> None:
        self.chunks.append(chunk)

    def text(self, s: str) -> None:
        """Convenience for the common case."""
        self.send(Text(s))

    def is_closed(self) -> bool:
        """Always ``False`` here.

        The Rust sink reports a disconnected client so a long generation can
        abandon a turn nobody is listening to. The buffered harness has no such
        signal — nothing is sent until the handler returns — so a provider
        cannot observe a mid-turn disconnect and this is always open.
        """
        return False


class LlmProvider:
    """What an app implements to become a model. Subclass and override."""

    def models(self) -> list[ModelCard]:
        """Every model this app can serve, right now."""
        raise NotImplementedError

    def chat(self, req: ChatRequest, sink: ChunkSink) -> None:
        """Run one turn, sending events to ``sink`` as they happen.

        Raising after events have already been sent ends the turn early; the
        client keeps what it received (a stream gets an error chunk, a
        non-stream request gets a 500). Load weights here, lazily — **not** at
        startup. The daemon health-gates a newly spawned app on a 30-second
        budget with a 5-second probe, so an app that loads gigabytes before it
        binds its port is reported as failing to start, with nothing in the
        error to say that loading was the reason.
        """
        raise NotImplementedError


# ---------------------------------------------------------------------------
# Rendering — pure, and unit-testable without a server
# ---------------------------------------------------------------------------


def render_stream_chunk(
    stream_id: str, model: str, chunk: Chunk, tool_index: int
) -> tuple[dict[str, Any] | None, int]:
    """Render one :class:`Chunk` as a ``chat.completion.chunk`` dict.

    Returns ``(payload, next_tool_index)``. A ``None`` payload means the chunk
    produces nothing on the wire (empty text or reasoning) and the index is
    unchanged.

    The tool-call shape is the fiddly part and the reason this is not left to
    apps. SenClaw's OpenAI adapter accumulates ``function.name`` and
    ``function.arguments`` by **concatenation** across chunks keyed on
    ``index`` — so a whole call must go out as a single delta at a *fresh*
    index. Reuse index 0 for a second call and the consumer welds them into
    ``get_weatherget_time`` with both argument objects glued together. That is
    why this threads ``tool_index`` and bumps it per :class:`ToolCall` rather
    than hardcoding 0.
    """
    if isinstance(chunk, Text):
        if not chunk.text:
            return None, tool_index
        delta: dict[str, Any] = {"content": chunk.text}
    elif isinstance(chunk, Reasoning):
        if not chunk.text:
            return None, tool_index
        delta = {"reasoning_content": chunk.text}
    elif isinstance(chunk, ToolCall):
        i = tool_index
        tool_index += 1
        delta = {
            "tool_calls": [
                {
                    "index": i,
                    "id": chunk.id,
                    "type": "function",
                    "function": {"name": chunk.name, "arguments": chunk.arguments},
                }
            ]
        }
    elif isinstance(chunk, Usage):
        # Usage rides its own chunk with an empty `choices` array — the shape
        # `stream_options.include_usage` produces, and the only place the
        # consumer looks for it.
        return {
            "id": stream_id,
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [],
            "usage": _usage_body(chunk),
        }, tool_index
    else:  # pragma: no cover - defensive: an unknown Chunk subclass
        raise TypeError(f"not a Chunk: {chunk!r}")

    return {
        "id": stream_id,
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
    }, tool_index


def sse_body(model: str, chunks: list[Chunk], *, error: str | None = None) -> str:
    """Assemble a full SSE body: one ``data: {json}`` event per rendered chunk,
    terminated by ``data: [DONE]``.

    ``[DONE]`` is sent **even when the generation failed**. A failure appends an
    error chunk (``finish_reason: "error"``) *before* the terminator rather than
    truncating silently: the client already has whatever text arrived, and an
    unterminated stream reads as a hang that only the read timeout ends, not as
    a failure the caller can act on.
    """
    stream_id = completion_id()
    tool_index = 0
    out: list[str] = []
    for chunk in chunks:
        payload, tool_index = render_stream_chunk(stream_id, model, chunk, tool_index)
        if payload is not None:
            out.append(_sse_event(json.dumps(payload)))
    if error is not None:
        out.append(
            _sse_event(
                json.dumps(
                    {
                        "id": stream_id,
                        "object": "chat.completion.chunk",
                        "model": model,
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "error"}],
                        "error": {"message": error, "type": "server_error"},
                    }
                )
            )
        )
    out.append(_sse_event("[DONE]"))
    return "".join(out)


def non_stream_body(model: str, chunks: list[Chunk]) -> dict[str, Any]:
    """Assemble one ``chat.completion`` object from a whole generation.

    Text deltas are concatenated, reasoning is omitted when empty rather than
    sent blank, and ``finish_reason`` is ``tool_calls`` when any tool call was
    emitted, else ``stop`` — the distinction the caller routes on.
    """
    text, reasoning, calls, usage = _accumulate(chunks)
    message: dict[str, Any] = {"role": "assistant", "content": text}
    if reasoning:
        message["reasoning_content"] = reasoning
    if calls:
        message["tool_calls"] = calls
    out: dict[str, Any] = {
        "id": completion_id(),
        "object": "chat.completion",
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": message,
                "finish_reason": "tool_calls" if calls else "stop",
            }
        ],
    }
    if usage is not None:
        out["usage"] = usage
    return out


def completion_id() -> str:
    """``chatcmpl-<hex>``. Uniqueness only has to hold within one client's
    stream, so process id plus a monotonic counter is enough and pulls in no
    dependency."""
    return f"chatcmpl-{os.getpid():x}{next(_COUNTER):x}"


_COUNTER = itertools.count()


def _accumulate(
    chunks: list[Chunk],
) -> tuple[str, str, list[dict[str, Any]], dict[str, Any] | None]:
    text: list[str] = []
    reasoning: list[str] = []
    calls: list[dict[str, Any]] = []
    usage: dict[str, Any] | None = None
    for chunk in chunks:
        if isinstance(chunk, Text):
            text.append(chunk.text)
        elif isinstance(chunk, Reasoning):
            reasoning.append(chunk.text)
        elif isinstance(chunk, ToolCall):
            calls.append(
                {
                    "id": chunk.id,
                    "type": "function",
                    "function": {"name": chunk.name, "arguments": chunk.arguments},
                }
            )
        elif isinstance(chunk, Usage):
            usage = _usage_body(chunk)
    return "".join(text), "".join(reasoning), calls, usage


def _usage_body(chunk: Usage) -> dict[str, Any]:
    return {
        "prompt_tokens": chunk.prompt_tokens,
        "completion_tokens": chunk.completion_tokens,
        "total_tokens": chunk.prompt_tokens + chunk.completion_tokens,
    }


def _sse_event(data: str) -> str:
    # One SSE event: a `data:` field terminated by a blank line. `json.dumps`
    # emits no newlines, so a payload is always a single `data:` line.
    return f"data: {data}\n\n"


def _as_int(v: Any) -> int | None:
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    return int(v)


def _as_float(v: Any) -> float | None:
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    return float(v)


def _error_response(status: int, message: str) -> Response:
    return Response(
        {"error": {"message": message, "type": "invalid_request_error"}}, status=status
    )


# ---------------------------------------------------------------------------
# The routes
# ---------------------------------------------------------------------------


def llm_routes(
    provider: LlmProvider, prefix: str = "/v1"
) -> dict[tuple[str, str], Callable[[Request], Response]]:
    """Build ``GET {prefix}/models`` + ``POST {prefix}/chat/completions``,
    ready to merge into :func:`senclaw_space.serve`.

        serve(routes={**llm_routes(provider), ("GET", "/health"): health})

    Mount at whatever the manifest's ``llm.path`` says — usually ``/v1``.
    """
    prefix = "/" + prefix.strip("/")

    def list_models(_req: Request) -> Response:
        data = []
        for m in provider.models():
            entry = m.to_json()
            # OpenAI envelope fields the daemon ignores but a plain OpenAI
            # client expects; the capability fields ride alongside from to_json.
            entry["object"] = "model"
            entry["owned_by"] = "senclaw-space-app"
            data.append(entry)
        return Response({"object": "list", "data": data})

    def chat_completions(req: Request) -> Response:
        try:
            body = req.json()
        except (ValueError, json.JSONDecodeError):
            return _error_response(400, "request body must be valid JSON")
        try:
            chat_req = ChatRequest.from_body(body)
        except ValueError as exc:
            return _error_response(400, str(exc))
        if not any(m.id == chat_req.model for m in provider.models()):
            return _error_response(404, f"unknown model `{chat_req.model}`")

        # Buffered: run the whole generation, collecting chunks, then render.
        sink = ChunkSink()
        error: str | None = None
        try:
            provider.chat(chat_req, sink)
        except Exception as exc:  # noqa: BLE001 - reported to the client, below
            error = str(exc)

        if chat_req.stream:
            # A stream request always gets a stream response — even on failure.
            # The client asked for SSE and parses SSE; a 500 here would arrive
            # as a body it does not decode. The error travels as a chunk, and
            # `[DONE]` still terminates the stream.
            return Response(
                sse_body(chat_req.model, sink.chunks, error=error),
                content_type="text/event-stream",
            )

        # Non-stream: a partial answer plus a crash is indistinguishable from a
        # short reply, so a failed generation is a 500 that discards the partial
        # rather than a 200 the caller cannot tell apart.
        if error is not None:
            return _error_response(500, error)
        return Response(non_stream_body(chat_req.model, sink.chunks))

    return {
        ("GET", f"{prefix}/models"): list_models,
        ("POST", f"{prefix}/chat/completions"): chat_completions,
    }


# ---------------------------------------------------------------------------
# Model cache
# ---------------------------------------------------------------------------


def publish_models(app_dir: str | os.PathLike[str], models: list[ModelCard]) -> None:
    """Write the model list to :data:`MODELS_CACHE_PATH`, for the daemon to read
    while this app is stopped. Call it once at startup, after the models are
    known.

    An empty list is **refused** rather than written. The daemon treats a
    missing cache as "not known yet" and a present one as authoritative, so
    clobbering a good list with an empty one during a failed startup would
    remove the app's models from the picker until someone noticed.
    """
    if not models:
        raise ValueError("refusing to publish an empty model list")
    path = Path(app_dir) / MODELS_CACHE_PATH
    path.parent.mkdir(parents=True, exist_ok=True)
    body = json.dumps({"models": [m.to_json() for m in models]}, indent=2)
    # Write-then-rename: a daemon reading this file concurrently sees either the
    # old list or the new one, never a truncated one.
    tmp = path.parent / (path.name + ".tmp")
    tmp.write_text(body, encoding="utf-8")
    os.replace(tmp, path)


__all__ = [
    "MODELS_CACHE_PATH",
    "ModelCard",
    "ChatRequest",
    "Chunk",
    "Text",
    "Reasoning",
    "ToolCall",
    "Usage",
    "ChunkSink",
    "LlmProvider",
    "render_stream_chunk",
    "sse_body",
    "non_stream_body",
    "completion_id",
    "llm_routes",
    "publish_models",
]
