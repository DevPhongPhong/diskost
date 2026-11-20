# Diskost meeting protocol

Version 1. Clients and the server exchange one JSON object per WebSocket text frame.

Direct replies echo the client's `req`. Broadcasts omit `req`. A client treats `joined`, `left`, and `error` with a matching non-zero `req` as the answer to the command it just sent.

## Client to server

| type | fields |
| --- | --- |
| `create_room` | `req`, `name`, `display_name`, `passcode` |
| `join_room` | `req`, `room_id`, `display_name`, `passcode` |
| `leave` | `req` |
| `chat` | `req`, `text` |
| `signal` | `req`, `to` (participant id or `*`), `payload` object |
| `media_state` | `req`, `audio`, `video`, `screen` |
| `ping` | `req`, `nonce` |

## Server to client

| type | fields |
| --- | --- |
| `joined` | `req`, `room_id`, `room_name`, `self_id`, `created`, `participants`, `ice_servers` |
| `participant_joined` | `participant` |
| `participant_left` | `participant_id` |
| `chat_message` | `from`, `display_name`, `text`, `unix_ms` |
| `signal` | `from`, `payload` |
| `media_state` | `participant_id`, `audio`, `video`, `screen` |
| `left` | `req` |
| `pong` | `req`, `nonce` |
| `error` | `req`, `code`, `message` |

A participant object is `{ "id", "display_name", "audio", "video", "screen" }`.

## WebRTC payload

Peers put this object in `signal.payload`. The participant with the greater id sends the offer.

```json
{ "kind": "offer", "sdp": "..." }
{ "kind": "answer", "sdp": "..." }
{ "kind": "ice", "candidate": "...", "sdp_mid": "0", "sdp_mline_index": 0 }
```

Signaling is cleartext WebSocket. The media path, once a peer connection exists, is DTLS-SRTP as usual for WebRTC.
