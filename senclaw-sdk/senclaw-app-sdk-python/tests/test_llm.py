"""Tests for :mod:`senclaw_space.llm` — an app serving a model.

Ported field-for-field from the Rust SDK's ``llm.rs`` test module, because the
bytes on the wire are the contract SenClaw's OpenAI adapter parses. The
load-bearing one is the tool-call index: the adapter accumulates
``delta.tool_calls[].function.{name,arguments}`` by *concatenation* keyed on
``index``, so a second call reusing index 0 produces ``get_weatherget_time``
with both argument objects glued together — and no test of the event builder in
isolation would notice.

Run: ``python -m pytest senclaw-sdk/senclaw-app-sdk-python/tests/test_llm.py``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from senclaw_space.llm import (  # noqa: E402
    MODELS_CACHE_PATH,
    ChatRequest,
    ChunkSink,
    LlmProvider,
    ModelCard,
    Reasoning,
    Text,
    ToolCall,
    Usage,
    llm_routes,
    non_stream_body,
    publish_models,
    render_stream_chunk,
    sse_body,
)
from senclaw_space.server import Request  # noqa: E402


def card() -> ModelCard:
    return ModelCard("m", 4096, 512, vision=False)


# ---------------------------------------------------------------------------
# Request parsing
# ---------------------------------------------------------------------------


def test_a_request_without_messages_is_refused_rather_than_run():
    for bad in ({"model": "m"}, {"model": "m", "messages": []}, {"messages": [{"role": "user"}]}):
        try:
            ChatRequest.from_body(bad)
        except ValueError:
            pass
        else:
            raise AssertionError(f"{bad!r} must be refused")


def test_both_spellings_of_the_output_ceiling_are_read():
    msgs = [{"role": "user", "content": "hi"}]
    a = ChatRequest.from_body({"model": "m", "messages": msgs, "max_tokens": 100})
    assert a.max_tokens == 100

    # The newer spelling wins when a client sends both.
    b = ChatRequest.from_body(
        {"model": "m", "messages": msgs, "max_tokens": 100, "max_completion_tokens": 200}
    )
    assert b.max_tokens == 200


def test_multipart_content_survives_the_request_parse():
    # Image turns arrive as an array of content parts. Re-encoding through a
    # typed message would flatten that to a string and drop the image.
    req = ChatRequest.from_body(
        {
            "model": "m",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "what is this"},
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
                    ],
                }
            ],
        }
    )
    parts = req.messages[0]["content"]
    assert len(parts) == 2
    assert parts[1]["image_url"]["url"] == "data:image/png;base64,AAA"


# ---------------------------------------------------------------------------
# Chunk rendering
# ---------------------------------------------------------------------------


def test_empty_text_or_reasoning_emits_no_chunk_at_all():
    assert render_stream_chunk("id", "m", Text(""), 0)[0] is None
    assert render_stream_chunk("id", "m", Reasoning(""), 0)[0] is None
    # A non-empty one does render, and does not consume a tool index.
    payload, idx = render_stream_chunk("id", "m", Text("hi"), 0)
    assert payload["choices"][0]["delta"] == {"content": "hi"}
    assert idx == 0


def test_a_usage_chunk_has_empty_choices_and_top_level_usage():
    # The shape `stream_options.include_usage` produces, and the only place the
    # consumer looks for it.
    payload, idx = render_stream_chunk("id", "m", Usage(12, 3), 0)
    assert payload["choices"] == []
    assert payload["usage"]["prompt_tokens"] == 12
    assert payload["usage"]["total_tokens"] == 15
    assert idx == 0, "usage does not consume a tool-call index"


def test_reasoning_is_omitted_when_empty_rather_than_sent_blank():
    body = non_stream_body("m", [Text("hi")])
    assert "reasoning_content" not in body["choices"][0]["message"]
    body = non_stream_body("m", [Reasoning("why"), Text("hi")])
    assert body["choices"][0]["message"]["reasoning_content"] == "why"


def test_a_tool_call_turn_finishes_as_tool_calls_not_stop():
    calls = non_stream_body("m", [ToolCall("c1", "f", "{}")])
    assert calls["choices"][0]["finish_reason"] == "tool_calls"
    assert non_stream_body("m", [Text("hi")])["choices"][0]["finish_reason"] == "stop"


# ---------------------------------------------------------------------------
# End-to-end over the real routes — the bytes are the contract
# ---------------------------------------------------------------------------


class Fake(LlmProvider):
    def __init__(self, chunks, boom=None):
        self._chunks = chunks
        self._boom = boom

    def models(self):
        return [card()]

    def chat(self, req, sink):
        for c in self._chunks:
            sink.send(c)
        if self._boom is not None:
            raise RuntimeError(self._boom)


def post_chat(chunks, stream, *, model="m", boom=None):
    routes = llm_routes(Fake(chunks, boom=boom))
    handler = routes[("POST", "/v1/chat/completions")]
    body = {"model": model, "messages": [{"role": "user", "content": "hi"}], "stream": stream}
    return handler(Request("POST", "/v1/chat/completions", {}, json.dumps(body).encode(), {}))


def sse_payloads(raw: str) -> list:
    """Parse an SSE body back into the JSON objects a consumer would see."""
    out = []
    for line in raw.splitlines():
        if line.startswith("data: "):
            data = line[len("data: ") :]
            if data != "[DONE]":
                out.append(json.loads(data))
    return out


def test_two_tool_calls_stream_at_distinct_indices():
    resp = post_chat(
        [
            ToolCall("call_a", "get_weather", '{"city":"Hanoi"}'),
            ToolCall("call_b", "get_time", "{}"),
        ],
        stream=True,
    )
    assert resp.content_type == "text/event-stream"
    calls = [
        p["choices"][0]["delta"]["tool_calls"][0]
        for p in sse_payloads(resp.body.decode())
        if p["choices"] and p["choices"][0]["delta"].get("tool_calls")
    ]
    assert len(calls) == 2
    assert calls[0]["index"] == 0
    assert calls[0]["function"]["name"] == "get_weather"
    assert calls[1]["index"] == 1, "a reused index welds the two calls together"
    assert calls[1]["function"]["name"] == "get_time"


def test_a_stream_always_terminates_with_done():
    resp = post_chat([Text("hi")], stream=True)
    raw = resp.body.decode()
    assert raw.rstrip().endswith("data: [DONE]"), f"unterminated stream reads as a hang:\n{raw}"


def test_usage_arrives_on_its_own_chunk_with_no_choices_over_the_wire():
    resp = post_chat([Text("hi"), Usage(12, 3)], stream=True)
    usage = next(p for p in sse_payloads(resp.body.decode()) if "usage" in p)
    assert usage["usage"]["prompt_tokens"] == 12
    assert usage["usage"]["total_tokens"] == 15
    assert usage["choices"] == []


def test_non_stream_returns_one_assembled_message():
    resp = post_chat([Reasoning("thinking"), Text("he"), Text("llo")], stream=False)
    body = json.loads(resp.body)
    msg = body["choices"][0]["message"]
    assert msg["content"] == "hello", "text deltas must be concatenated"
    assert msg["reasoning_content"] == "thinking"
    assert body["object"] == "chat.completion"


def test_an_unknown_model_is_404_not_a_silent_default():
    resp = post_chat([], stream=True, model="nope")
    assert resp.status == 404


def test_a_bad_request_is_400():
    # A body the parser rejects (no messages) is a 400 before the provider runs.
    routes = llm_routes(Fake([]))
    handler = routes[("POST", "/v1/chat/completions")]
    resp = handler(Request("POST", "/v1/chat/completions", {}, json.dumps({"model": "m"}).encode(), {}))
    assert resp.status == 400


def test_a_provider_error_is_500_when_not_streaming():
    resp = post_chat([Text("partial")], stream=False, boom="weights failed to load")
    assert resp.status == 500
    assert "weights failed to load" in json.loads(resp.body)["error"]["message"]


def test_a_provider_error_streams_an_error_chunk_then_done():
    # The status line is committed to SSE the moment the client asked for it, so
    # a failure cannot become a 5xx — it must be an error chunk, then `[DONE]`.
    resp = post_chat([Text("partial")], stream=True, boom="kaboom")
    assert resp.content_type == "text/event-stream"
    raw = resp.body.decode()
    err = next(p for p in sse_payloads(raw) if p.get("error"))
    assert err["choices"][0]["finish_reason"] == "error"
    assert "kaboom" in err["error"]["message"]
    assert raw.rstrip().endswith("data: [DONE]")


def test_models_endpoint_carries_the_capability_fields():
    # The daemon builds a picker entry from this, so `vision` must survive the
    # hop — it decides between real image blocks and the OCR fallback.
    routes = llm_routes(Fake([]))
    resp = routes[("GET", "/v1/models")](Request("GET", "/v1/models", {}, b"", {}))
    m = json.loads(resp.body)["data"][0]
    assert m["id"] == "m"
    assert m["context_length"] == 4096
    assert m["max_output_tokens"] == 512
    assert m["vision"] is False
    assert m["tools"] is True
    assert m["object"] == "model"


def test_the_prefix_is_configurable():
    routes = llm_routes(Fake([]), prefix="openai/v1")
    assert ("POST", "/openai/v1/chat/completions") in routes
    assert ("GET", "/openai/v1/models") in routes


# ---------------------------------------------------------------------------
# The model cache
# ---------------------------------------------------------------------------


def test_an_empty_model_list_never_clobbers_a_good_cache(tmp_path):
    publish_models(tmp_path, [card()])
    good = (tmp_path / MODELS_CACHE_PATH).read_text(encoding="utf-8")

    try:
        publish_models(tmp_path, [])
    except ValueError:
        pass
    else:
        raise AssertionError("an empty publish must be refused")

    after = (tmp_path / MODELS_CACHE_PATH).read_text(encoding="utf-8")
    assert good == after, "a failed publish must leave the cache intact"


def test_published_cards_round_trip(tmp_path):
    m = ModelCard("gemma", 128_000, 8192, vision=True, display_name="Gemma 4")
    publish_models(tmp_path, [m])
    back = json.loads((tmp_path / MODELS_CACHE_PATH).read_text(encoding="utf-8"))
    cards = [ModelCard.from_json(c) for c in back["models"]]
    assert cards[0].id == "gemma"
    assert cards[0].display_name == "Gemma 4"
    assert cards[0].vision is True
    assert cards[0].tools is True


def test_tools_defaults_to_true_when_absent_from_json():
    # A card written before the `tools` field existed must not read as
    # chat-only — the daemon would hide the model's tool support.
    assert ModelCard.from_json({"id": "x", "context_length": 1, "max_output_tokens": 1}).tools is True
    assert ModelCard.from_json({"id": "x", "vision": True, "tools": False}).tools is False


# ---------------------------------------------------------------------------
# The sink
# ---------------------------------------------------------------------------


def test_the_sink_collects_and_reports_open():
    sink = ChunkSink()
    sink.text("a")
    sink.send(Text("b"))
    assert [c.text for c in sink.chunks] == ["a", "b"]
    # The buffered harness has no client-disconnect signal, so the sink is
    # always open — a provider cannot poll it to abandon a turn early.
    assert sink.is_closed() is False
