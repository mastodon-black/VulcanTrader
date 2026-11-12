/*
 * websocket.dll replacement for Wine: client side of the Windows
 * "WebSocket Protocol Component API", enough for .NET Framework 4.x
 * System.Net.WebSockets.ClientWebSocket (Wine's builtin is a stub, see
 * Wine bug 54623).
 *
 * The contract was taken from the consumer side in .NET reference source
 * (WebSocketProtocolComponent.cs, WebSocketBase.cs, WebSocketBuffer.cs):
 *   - .NET performs the real HTTP upgrade itself; the DLL only runs a fake
 *     handshake (must hand back Sec-WebSocket-Key / Sec-WebSocket-Version).
 *   - Every buffer returned by WebSocketGetAction must lie inside the
 *     caller-allocated native buffer (recv + send + 144 bytes), or inside
 *     the caller's pinned send payload.
 *   - Send queue gets 2 buffer slots (framing header + payload), receive
 *     queue gets 1 (so a PONG reply must be one contiguous buffer).
 *   - All calls arrive under .NET's SessionHandle lock; we lock anyway.
 *
 * Client frames are masked with an all-zero masking key. That is still a
 * masked frame per RFC 6455 and is what .NET's own useZeroMaskingKey mode
 * produces; it lets the caller's payload go to the wire unmodified, so each
 * frame is exactly one SendToNetwork action (no interleaving with PONGs).
 *
 * Server-side handshake functions are not implemented (E_NOTIMPL).
 */
#include <windows.h>
#include <string.h>
#include <stdio.h>
#include <stdarg.h>

#define WS_E_INVALID_OPERATION          ((HRESULT)0x80000050L)
#define WS_E_INVALID_PROTOCOL_OPERATION ((HRESULT)0x80000051L)
#define WS_E_INVALID_PROTOCOL_FORMAT    ((HRESULT)0x80000052L)

enum { PROP_RECV_BUF = 0, PROP_SEND_BUF = 1, PROP_DISABLE_MASK = 2, PROP_ALLOC_BUF = 3,
       PROP_DISABLE_UTF8 = 4, PROP_KEEPALIVE = 5, PROP_VERSIONS = 6 };
enum { ACT_NONE = 0, ACT_SEND_TO_NET = 1, ACT_IND_SEND_DONE = 2, ACT_RECV_FROM_NET = 3,
       ACT_IND_RECV_DONE = 4 };
enum { Q_SEND = 1, Q_RECV = 2 };

#define BT_NONE      0x00000000u
#define BT_UTF8_MSG  0x80000000u
#define BT_UTF8_FRAG 0x80000001u
#define BT_BIN_MSG   0x80000002u
#define BT_BIN_FRAG  0x80000003u
#define BT_CLOSE     0x80000004u
#define BT_PINGPONG  0x80000005u
#define BT_UPONG     0x80000006u

typedef struct { int Type; void *pvValue; ULONG ulValueSize; } WS_PROPERTY;
typedef struct { char *pcName; ULONG ulNameLength; char *pcValue; ULONG ulValueLength; } WS_HTTP_HEADER;
typedef union {
    struct { BYTE *pbBuffer; ULONG ulBufferLength; } Data;
    struct { BYTE *pbReason; ULONG ulReasonLength; USHORT usStatus; } CloseStatus;
} WS_BUFFER;

#define OVERHEAD      144
#define SESSION_MAGIC 0x57534F4Bu /* 'WSOK' */
#define CTX_SEND      ((void *)(ULONG_PTR)0x5E4D)
#define CTX_RECV      ((void *)(ULONG_PTR)0x4ECF)

enum { S_IDLE = 0, S_QUEUED, S_SENDING, S_SENT, S_INDICATING };
enum { R_NONE = 0, R_READING, R_INDICATING, R_PONGING };

typedef struct ws_session {
    DWORD magic;
    CRITICAL_SECTION cs;
    BYTE *base;
    ULONG recv_size, send_size;
    BOOL own_buf, aborted;

    char key[32];
    WS_HTTP_HEADER hdrs[2];

    /* send side */
    int s_state;
    ULONG s_type;
    BYTE *s_hdr;            /* inside native buffer: base + recv_size */
    ULONG s_hdr_len;
    BYTE *s_payload;        /* caller's pinned buffer (unmodified) */
    ULONG s_payload_len;
    BOOL s_in_msg;          /* inside a fragmented outgoing message */

    /* receive side */
    BOOL r_pending;
    int r_action;
    BYTE *rbuf;             /* base, recv_size bytes */
    ULONG rpos, rlen;
    BOOL r_in_frame, r_fin, r_masked, r_in_msg, r_text;
    ULONGLONG r_remaining, r_mask_off;
    BYTE r_mask[4];
    BYTE *pong;             /* base + recv_size + send_size, OVERHEAD bytes */
    ULONG pong_len;
    BOOL pong_pending;
} ws_session;

/* ---- optional tracing: set WSDLL_TRACE_FILE to a Windows path ---- */
static void trace(const char *fmt, ...)
{
    static int enabled = -1;
    static char path[MAX_PATH];
    char line[512];
    va_list ap;
    HANDLE f;
    DWORD n;
    int len;

    if (enabled == 0) return;
    if (enabled < 0) {
        enabled = GetEnvironmentVariableA("WSDLL_TRACE_FILE", path, sizeof(path)) > 0;
        if (!enabled) return;
    }
    len = _snprintf(line, sizeof(line) - 2, "[%lu] ", GetCurrentThreadId());
    va_start(ap, fmt);
    len += _vsnprintf(line + len, sizeof(line) - 2 - len, fmt, ap);
    va_end(ap);
    if (len < 0 || len > (int)sizeof(line) - 2) len = sizeof(line) - 2;
    line[len++] = '\n';
    f = CreateFileA(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                    OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) return;
    WriteFile(f, line, len, &n, NULL);
    CloseHandle(f);
}

static ws_session *lock_session(void *h)
{
    ws_session *s = h;
    if (!s || s->magic != SESSION_MAGIC) return NULL;
    EnterCriticalSection(&s->cs);
    return s;
}

static void unlock_session(ws_session *s) { LeaveCriticalSection(&s->cs); }

static ULONG build_header(BYTE *h, BYTE fin_opcode, ULONGLONG len)
{
    ULONG n = 0;
    int i;
    h[n++] = fin_opcode;
    if (len < 126) {
        h[n++] = 0x80 | (BYTE)len;
    } else if (len <= 0xFFFF) {
        h[n++] = 0x80 | 126;
        h[n++] = (BYTE)(len >> 8);
        h[n++] = (BYTE)len;
    } else {
        h[n++] = 0x80 | 127;
        for (i = 7; i >= 0; i--) h[n++] = (BYTE)(len >> (8 * i));
    }
    h[n++] = 0; h[n++] = 0; h[n++] = 0; h[n++] = 0;   /* zero masking key */
    return n;
}

static void unmask(BYTE *p, ULONG n, const BYTE *mask, ULONGLONG off)
{
    ULONG i;
    for (i = 0; i < n; i++) p[i] ^= mask[(off + i) & 3];
}

static ULONG data_type(BOOL text, BOOL final)
{
    if (text) return final ? BT_UTF8_MSG : BT_UTF8_FRAG;
    return final ? BT_BIN_MSG : BT_BIN_FRAG;
}

static void make_key(char *out)
{
    static const char b64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    BYTE raw[18];
    LARGE_INTEGER qpc;
    ULONGLONG x;
    int i, o = 0;

    QueryPerformanceCounter(&qpc);
    x = (ULONGLONG)qpc.QuadPart ^ ((ULONGLONG)GetTickCount64() << 21) ^ (ULONG_PTR)out;
    for (i = 0; i < 16; i++) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        raw[i] = (BYTE)(x >> 24);
    }
    raw[16] = raw[17] = 0;
    for (i = 0; i < 18; i += 3) {
        ULONG v = (raw[i] << 16) | (raw[i + 1] << 8) | raw[i + 2];
        out[o++] = b64[(v >> 18) & 63];
        out[o++] = b64[(v >> 12) & 63];
        out[o++] = b64[(v >> 6) & 63];
        out[o++] = b64[v & 63];
    }
    out[22] = '=';
    out[23] = '=';
    out[24] = 0;
}

/* ------------------------------------------------------------------ */

HRESULT WINAPI WebSocketCreateClientHandle(const WS_PROPERTY *props, ULONG count, void **handle)
{
    ULONG recv_size = 4096, send_size = 4096, alloc_size = 0, need, i;
    BYTE *alloc = NULL;
    ws_session *s;

    trace("CreateClientHandle(%p, %lu, %p)", props, count, handle);
    if (!handle) return E_INVALIDARG;
    *handle = NULL;
    if (count && !props) return E_INVALIDARG;

    for (i = 0; i < count; i++) {
        const WS_PROPERTY *p = &props[i];
        switch (p->Type) {
        case PROP_RECV_BUF:
            if (!p->pvValue || p->ulValueSize < sizeof(ULONG)) return E_INVALIDARG;
            recv_size = *(ULONG *)p->pvValue;
            break;
        case PROP_SEND_BUF:
            if (!p->pvValue || p->ulValueSize < sizeof(ULONG)) return E_INVALIDARG;
            send_size = *(ULONG *)p->pvValue;
            break;
        case PROP_ALLOC_BUF:
            /* pvValue points at a pointer to the buffer; ulValueSize is its size */
            if (!p->pvValue) return E_INVALIDARG;
            alloc = *(BYTE **)p->pvValue;
            alloc_size = p->ulValueSize;
            break;
        default:
            break; /* masking / utf8 / keepalive: accepted, no effect */
        }
    }
    if (recv_size < 256 || recv_size > 65536 || send_size < 16 || send_size > 65536)
        return E_INVALIDARG;
    need = recv_size + send_size + OVERHEAD;
    if (alloc && alloc_size < need) return E_INVALIDARG;

    s = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(*s));
    if (!s) return E_OUTOFMEMORY;
    if (alloc) {
        s->base = alloc;
    } else {
        s->base = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, need);
        if (!s->base) { HeapFree(GetProcessHeap(), 0, s); return E_OUTOFMEMORY; }
        s->own_buf = TRUE;
    }
    s->recv_size = recv_size;
    s->send_size = send_size;
    s->rbuf = s->base;
    s->s_hdr = s->base + recv_size;
    s->pong = s->base + recv_size + send_size;
    InitializeCriticalSection(&s->cs);
    s->magic = SESSION_MAGIC;
    *handle = s;
    trace("  -> %p recv=%lu send=%lu alloc=%p/%lu", s, recv_size, send_size, alloc, alloc_size);
    return S_OK;
}

HRESULT WINAPI WebSocketBeginClientHandshake(void *h, const char **subprotocols, ULONG nsub,
        const char **extensions, ULONG next, const WS_HTTP_HEADER *initial, ULONG ninitial,
        WS_HTTP_HEADER **additional, ULONG *nadditional)
{
    ws_session *s = lock_session(h);
    trace("BeginClientHandshake(%p)", h);
    if (!s) return E_INVALIDARG;
    if (!additional || !nadditional) { unlock_session(s); return E_INVALIDARG; }
    make_key(s->key);
    s->hdrs[0].pcName = (char *)"Sec-WebSocket-Key";
    s->hdrs[0].ulNameLength = 17;
    s->hdrs[0].pcValue = s->key;
    s->hdrs[0].ulValueLength = 24;
    s->hdrs[1].pcName = (char *)"Sec-WebSocket-Version";
    s->hdrs[1].ulNameLength = 21;
    s->hdrs[1].pcValue = (char *)"13";
    s->hdrs[1].ulValueLength = 2;
    *additional = s->hdrs;
    *nadditional = 2;
    unlock_session(s);
    return S_OK;
}

HRESULT WINAPI WebSocketEndClientHandshake(void *h, const WS_HTTP_HEADER *response, ULONG nresponse,
        ULONG *selected_ext, ULONG *nselected_ext, ULONG *selected_subprotocol)
{
    ws_session *s = lock_session(h);
    trace("EndClientHandshake(%p, %lu headers)", h, nresponse);
    if (!s) return E_INVALIDARG;
    if (nselected_ext) *nselected_ext = 0;
    unlock_session(s);
    return S_OK;
}

HRESULT WINAPI WebSocketCreateServerHandle(const WS_PROPERTY *props, ULONG count, void **handle)
{
    trace("CreateServerHandle: not implemented");
    if (handle) *handle = NULL;
    return E_NOTIMPL;
}

HRESULT WINAPI WebSocketBeginServerHandshake(void *h, const char *subprotocol, const char **ext,
        ULONG next, const WS_HTTP_HEADER *req, ULONG nreq, WS_HTTP_HEADER **resp, ULONG *nresp)
{
    return E_NOTIMPL;
}

HRESULT WINAPI WebSocketEndServerHandshake(void *h)
{
    return E_NOTIMPL;
}

VOID WINAPI WebSocketAbortHandle(void *h)
{
    ws_session *s = lock_session(h);
    trace("AbortHandle(%p)", h);
    if (!s) return;
    s->aborted = TRUE;
    unlock_session(s);
}

VOID WINAPI WebSocketDeleteHandle(void *h)
{
    ws_session *s = h;
    trace("DeleteHandle(%p)", h);
    if (!s || s->magic != SESSION_MAGIC) return;
    s->magic = 0;
    DeleteCriticalSection(&s->cs);
    if (s->own_buf) HeapFree(GetProcessHeap(), 0, s->base);
    HeapFree(GetProcessHeap(), 0, s);
}

HRESULT WINAPI WebSocketSend(void *h, ULONG type, WS_BUFFER *buf, void *app_ctx)
{
    ws_session *s = lock_session(h);
    HRESULT hr = S_OK;
    BYTE *payload = NULL;
    ULONG plen = 0, hl;

    if (!s) return E_INVALIDARG;
    if (s->aborted || s->s_state != S_IDLE) {
        trace("Send(%p, %08lx): invalid state aborted=%d s_state=%d", h, type, s->aborted, s->s_state);
        hr = WS_E_INVALID_OPERATION;
        goto done;
    }
    switch (type) {
    case BT_UTF8_MSG: case BT_UTF8_FRAG: case BT_BIN_MSG: case BT_BIN_FRAG: {
        BOOL text = (type == BT_UTF8_MSG || type == BT_UTF8_FRAG);
        BOOL fin = (type == BT_UTF8_MSG || type == BT_BIN_MSG);
        BYTE op = s->s_in_msg ? 0 : (text ? 1 : 2);
        if (buf) { payload = buf->Data.pbBuffer; plen = buf->Data.ulBufferLength; }
        if (plen && !payload) { hr = E_INVALIDARG; goto done; }
        hl = build_header(s->s_hdr, (BYTE)((fin ? 0x80 : 0) | op), plen);
        s->s_in_msg = !fin;
        break;
    }
    case BT_CLOSE:
        if (buf) {
            USHORT st = buf->CloseStatus.usStatus;
            plen = buf->CloseStatus.ulReasonLength;
            payload = plen ? buf->CloseStatus.pbReason : NULL;
            if (plen > 123 || (plen && !payload)) { hr = E_INVALIDARG; goto done; }
            hl = build_header(s->s_hdr, 0x88, 2 + plen);
            s->s_hdr[hl++] = (BYTE)(st >> 8);
            s->s_hdr[hl++] = (BYTE)st;
        } else {
            hl = build_header(s->s_hdr, 0x88, 0);
        }
        break;
    case BT_PINGPONG: case BT_UPONG:
        if (buf) { payload = buf->Data.pbBuffer; plen = buf->Data.ulBufferLength; }
        if (plen > 125 || (plen && !payload)) { hr = E_INVALIDARG; goto done; }
        hl = build_header(s->s_hdr, type == BT_PINGPONG ? 0x89 : 0x8A, plen);
        break;
    default:
        hr = E_INVALIDARG;
        goto done;
    }
    s->s_type = type;
    s->s_hdr_len = hl;
    s->s_payload = plen ? payload : NULL;
    s->s_payload_len = plen;
    s->s_state = S_QUEUED;
    trace("Send(%p, %08lx, %lu bytes)", h, type, plen);
done:
    unlock_session(s);
    return hr;
}

HRESULT WINAPI WebSocketReceive(void *h, WS_BUFFER *buf, void *app_ctx)
{
    ws_session *s = lock_session(h);
    HRESULT hr = S_OK;
    if (!s) return E_INVALIDARG;
    if (s->aborted) hr = WS_E_INVALID_OPERATION;
    else s->r_pending = TRUE;
    unlock_session(s);
    return hr;
}

static void get_send_action(ws_session *s, WS_BUFFER *bufs, ULONG cap, ULONG *count,
                            int *action, ULONG *btype, void **actctx)
{
    if (s->s_state == S_QUEUED && (cap >= 2 || (cap >= 1 && !s->s_payload_len))) {
        bufs[0].Data.pbBuffer = s->s_hdr;
        bufs[0].Data.ulBufferLength = s->s_hdr_len;
        *count = 1;
        if (s->s_payload_len) {
            bufs[1].Data.pbBuffer = s->s_payload;
            bufs[1].Data.ulBufferLength = s->s_payload_len;
            *count = 2;
        }
        *action = ACT_SEND_TO_NET;
        *btype = s->s_type;
        *actctx = CTX_SEND;
        s->s_state = S_SENDING;
    } else if (s->s_state == S_SENT) {
        *action = ACT_IND_SEND_DONE;
        *btype = s->s_type;
        *actctx = CTX_SEND;
        s->s_state = S_INDICATING;
    }
}

static HRESULT get_recv_action(ws_session *s, WS_BUFFER *bufs, ULONG cap, ULONG *count,
                               int *action, ULONG *btype, void **actctx)
{
    if (s->r_action != R_NONE || cap < 1) return S_OK;

    for (;;) {
        ULONG avail, n;
        BYTE *p;

        if (s->pong_pending) {
            bufs[0].Data.pbBuffer = s->pong;
            bufs[0].Data.ulBufferLength = s->pong_len;
            *count = 1;
            *action = ACT_SEND_TO_NET;
            *btype = BT_PINGPONG;
            *actctx = CTX_RECV;
            s->r_action = R_PONGING;
            return S_OK;
        }
        if (!s->r_pending) return S_OK;

        avail = s->rlen - s->rpos;
        p = s->rbuf + s->rpos;

        if (!s->r_in_frame) {
            BYTE b0, b1, op, mask[4] = {0, 0, 0, 0};
            BOOL fin, masked;
            ULONG l7, hl, o = 2;
            ULONGLONG len;
            int i;

            if (avail < 2) goto need_more;
            b0 = p[0]; b1 = p[1];
            fin = (b0 & 0x80) != 0;
            op = b0 & 0x0F;
            masked = (b1 & 0x80) != 0;
            l7 = b1 & 0x7F;
            if (b0 & 0x70) return WS_E_INVALID_PROTOCOL_FORMAT; /* no extensions negotiated */
            hl = 2 + (l7 == 126 ? 2 : (l7 == 127 ? 8 : 0)) + (masked ? 4 : 0);
            if (avail < hl) goto need_more;
            len = l7;
            if (l7 == 126) { len = ((ULONG)p[2] << 8) | p[3]; o = 4; }
            else if (l7 == 127) {
                len = 0;
                for (i = 0; i < 8; i++) len = (len << 8) | p[2 + i];
                o = 10;
                if (len >> 63) return WS_E_INVALID_PROTOCOL_FORMAT;
            }
            if (masked) memcpy(mask, p + o, 4);

            if (op & 0x8) {                      /* control frame */
                BYTE *pl = p + hl;
                if (!fin || len > 125) return WS_E_INVALID_PROTOCOL_FORMAT;
                if (avail < hl + len) goto need_more;
                if (masked) unmask(pl, (ULONG)len, mask, 0);
                s->rpos += hl + (ULONG)len;
                if (op == 0x9) {                 /* PING -> queue PONG on receive queue */
                    s->pong[0] = 0x8A;
                    s->pong[1] = 0x80 | (BYTE)len;
                    s->pong[2] = s->pong[3] = s->pong[4] = s->pong[5] = 0;
                    memcpy(s->pong + 6, pl, (ULONG)len);
                    s->pong_len = 6 + (ULONG)len;
                    s->pong_pending = TRUE;
                    trace("recv PING %lu -> PONG", (ULONG)len);
                    continue;
                }
                if (op == 0xA) {                 /* PONG */
                    bufs[0].Data.pbBuffer = len ? pl : NULL;
                    bufs[0].Data.ulBufferLength = (ULONG)len;
                    *count = 1;
                    *action = ACT_IND_RECV_DONE;
                    *btype = BT_PINGPONG;
                    *actctx = CTX_RECV;
                    s->r_action = R_INDICATING;
                    return S_OK;
                }
                if (op == 0x8) {                 /* CLOSE */
                    if (len == 1) return WS_E_INVALID_PROTOCOL_FORMAT;
                    bufs[0].CloseStatus.usStatus = len >= 2 ? (USHORT)((pl[0] << 8) | pl[1]) : 1005;
                    bufs[0].CloseStatus.pbReason = len > 2 ? pl + 2 : NULL;
                    bufs[0].CloseStatus.ulReasonLength = len > 2 ? (ULONG)len - 2 : 0;
                    *count = 1;
                    *action = ACT_IND_RECV_DONE;
                    *btype = BT_CLOSE;
                    *actctx = CTX_RECV;
                    s->r_action = R_INDICATING;
                    trace("recv CLOSE status=%u reason_len=%lu", bufs[0].CloseStatus.usStatus,
                          bufs[0].CloseStatus.ulReasonLength);
                    return S_OK;
                }
                return WS_E_INVALID_PROTOCOL_FORMAT;
            }

            if (op == 0) {
                if (!s->r_in_msg) return WS_E_INVALID_PROTOCOL_FORMAT;
            } else if (op == 1 || op == 2) {
                if (s->r_in_msg) return WS_E_INVALID_PROTOCOL_FORMAT;
                s->r_text = (op == 1);
            } else {
                return WS_E_INVALID_PROTOCOL_FORMAT;
            }
            s->r_in_msg = TRUE;
            s->r_fin = fin;
            s->r_remaining = len;
            s->r_masked = masked;
            memcpy(s->r_mask, mask, 4);
            s->r_mask_off = 0;
            s->rpos += hl;
            if (len == 0) {
                if (fin) s->r_in_msg = FALSE;
                bufs[0].Data.pbBuffer = NULL;
                bufs[0].Data.ulBufferLength = 0;
                *count = 1;
                *action = ACT_IND_RECV_DONE;
                *btype = data_type(s->r_text, fin);
                *actctx = CTX_RECV;
                s->r_action = R_INDICATING;
                return S_OK;
            }
            s->r_in_frame = TRUE;
            continue;
        }

        /* inside a data frame: hand over whatever payload is buffered */
        if (avail == 0) goto need_more;
        n = avail;
        if ((ULONGLONG)n > s->r_remaining) n = (ULONG)s->r_remaining;
        if (s->r_masked) unmask(p, n, s->r_mask, s->r_mask_off);
        s->r_mask_off += n;
        s->r_remaining -= n;
        s->rpos += n;
        {
            BOOL final = (s->r_remaining == 0) && s->r_fin;
            if (s->r_remaining == 0) {
                s->r_in_frame = FALSE;
                if (s->r_fin) s->r_in_msg = FALSE;
            }
            bufs[0].Data.pbBuffer = p;
            bufs[0].Data.ulBufferLength = n;
            *count = 1;
            *action = ACT_IND_RECV_DONE;
            *btype = data_type(s->r_text, final);
            *actctx = CTX_RECV;
            s->r_action = R_INDICATING;
        }
        return S_OK;

    need_more:
        if (s->rpos) {
            memmove(s->rbuf, s->rbuf + s->rpos, s->rlen - s->rpos);
            s->rlen -= s->rpos;
            s->rpos = 0;
        }
        if (s->rlen >= s->recv_size) return WS_E_INVALID_PROTOCOL_FORMAT; /* cannot happen: recv >= 256 */
        bufs[0].Data.pbBuffer = s->rbuf + s->rlen;
        bufs[0].Data.ulBufferLength = s->recv_size - s->rlen;
        *count = 1;
        *action = ACT_RECV_FROM_NET;
        *btype = BT_NONE;
        *actctx = CTX_RECV;
        s->r_action = R_READING;
        return S_OK;
    }
}

HRESULT WINAPI WebSocketGetAction(void *h, int queue, WS_BUFFER *bufs, ULONG *count, int *action,
                                  ULONG *btype, void **app_ctx, void **act_ctx)
{
    ws_session *s;
    ULONG cap, dummy_type;
    void *dummy_ctx;
    HRESULT hr = S_OK;

    if (!action || !count) return E_INVALIDARG;
    if (!btype) btype = &dummy_type;
    if (!act_ctx) act_ctx = &dummy_ctx;
    cap = bufs ? *count : 0;
    *count = 0;
    *action = ACT_NONE;
    *btype = BT_NONE;
    *act_ctx = NULL;
    if (app_ctx) *app_ctx = NULL;

    if (!(s = lock_session(h))) return E_INVALIDARG;
    if (s->aborted) {
        hr = WS_E_INVALID_OPERATION;
    } else {
        if (queue & Q_SEND) get_send_action(s, bufs, cap, count, action, btype, act_ctx);
        if (*action == ACT_NONE && (queue & Q_RECV))
            hr = get_recv_action(s, bufs, cap, count, action, btype, act_ctx);
    }
    if (FAILED(hr)) trace("GetAction(%p, q=%d) -> %08lx", h, queue, hr);
    unlock_session(s);
    return hr;
}

VOID WINAPI WebSocketCompleteAction(void *h, void *act_ctx, ULONG bytes)
{
    ws_session *s = lock_session(h);
    if (!s) return;
    if (act_ctx == CTX_SEND) {
        if (s->s_state == S_SENDING) {
            /* short write means .NET already failed the operation: reset */
            s->s_state = (bytes >= s->s_hdr_len + s->s_payload_len) ? S_SENT : S_IDLE;
            if (s->s_state == S_IDLE) s->s_payload = NULL;
        } else if (s->s_state == S_INDICATING) {
            s->s_state = S_IDLE;
            s->s_payload = NULL;
        }
    } else if (act_ctx == CTX_RECV) {
        switch (s->r_action) {
        case R_READING:
            if (bytes > s->recv_size - s->rlen) bytes = s->recv_size - s->rlen;
            s->rlen += bytes;
            break;
        case R_INDICATING:
            s->r_pending = FALSE;   /* .NET copies/buffers the whole indicated chunk */
            break;
        case R_PONGING:
            s->pong_pending = FALSE;
            break;
        }
        s->r_action = R_NONE;
    }
    unlock_session(s);
}

HRESULT WINAPI WebSocketGetGlobalProperty(int type, void *value, ULONG *size)
{
    if (type == PROP_KEEPALIVE) {
        if (!value || !size || *size < sizeof(ULONG)) return E_INVALIDARG;
        *(ULONG *)value = 30000;   /* Windows default: 30 s */
        *size = sizeof(ULONG);
        return S_OK;
    }
    return E_INVALIDARG;
}
