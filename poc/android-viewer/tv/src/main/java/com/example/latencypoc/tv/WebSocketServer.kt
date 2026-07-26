package com.example.latencypoc.tv

import android.util.Base64
import android.util.Log
import java.io.InputStream
import java.net.ServerSocket
import java.net.Socket
import java.security.MessageDigest
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

private const val TAG = "WebSocketServer"

/**
 * Minimal WebSocket server (RFC 6455) that listens (server role) on a fixed
 * TCP port for tcp-ws-relay to connect to, then reads the binary frames it
 * forwards and feeds their payloads to an [AnnexBNalSplitter] -- same
 * downstream handling as [TcpReceiver] uses for a direct TCP connection.
 *
 * This is the server-side counterpart of what [WebSocketReceiver] used to
 * do as a client: rather than Android connecting out to tcp-ws-relay, the
 * relay now connects out to Android's listening socket here (see
 * tcp-ws-relay's own --android-host/--android-port flags), matching the
 * original direct-TCP topology (Android listens, advertises itself via
 * mDNS/NSD, and the other side connects in) but with the relay's TCP<->
 * WebSocket translation still in the middle.
 *
 * Implemented from scratch (handshake + frame parsing) rather than pulling
 * in a WebSocket server library -- this project already hand-rolls its own
 * framing for the Annex-B NAL splitter and the RTP depacketizer it
 * replaced, and RFC 6455's server-side handshake is a single fixed-string
 * SHA-1+base64 computation, not something that benefits much from a
 * library dependency for a POC that only ever needs to accept binary
 * frames from one known peer.
 */
class WebSocketServer(
    private val port: Int,
    private val onNewConnection: () -> Unit,
    private val onNal: (ByteArray) -> Unit,
    private val stats: StreamStats,
) {
    private val running = AtomicBoolean(false)
    private var thread: Thread? = null
    private var serverSocket: ServerSocket? = null
    private var clientSocket: Socket? = null

    /** Blocks until the listening socket is bound (or binding failed) before returning. */
    fun start() {
        if (running.getAndSet(true)) return
        val boundLatch = CountDownLatch(1)
        thread = Thread({ runLoop(boundLatch) }, "ws-server").apply { start() }
        boundLatch.await(2, TimeUnit.SECONDS)
    }

    fun stop() {
        running.set(false)
        try {
            clientSocket?.close()
        } catch (e: Exception) { /* already closing */ }
        try {
            serverSocket?.close()
        } catch (e: Exception) { /* already closing */ }
        thread?.join(1000)
        thread = null
    }

    private fun runLoop(boundLatch: CountDownLatch) {
        try {
            serverSocket = ServerSocket(port).apply { reuseAddress = true }
            Log.i(TAG, "Listening for WebSocket connections on port $port")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to bind WebSocket port $port", e)
            running.set(false)
            return
        } finally {
            boundLatch.countDown()
        }

        // Accepts connections in a loop rather than just once -- if
        // tcp-ws-relay is stopped and restarted, this lets the same running
        // Android app pick up the new connection without needing to be
        // relaunched itself (same reasoning as TcpReceiver's accept loop).
        while (running.get()) {
            val socket = try {
                serverSocket?.accept() ?: break
            } catch (e: Exception) {
                if (running.get()) Log.w(TAG, "accept() error", e)
                break
            }

            Log.i(TAG, "Relay connected from ${socket.remoteSocketAddress}")
            clientSocket = socket
            stats.connected = true
            handleConnection(socket)
            stats.connected = false
            clientSocket = null
        }

        try {
            serverSocket?.close()
        } catch (e: Exception) { /* ignore */ }
        Log.i(TAG, "WebSocket server stopped")
    }

    private fun handleConnection(socket: Socket) {
        socket.tcpNoDelay = true
        try {
            if (!performHandshake(socket)) {
                Log.w(TAG, "WebSocket handshake failed")
                return
            }
        } catch (e: Exception) {
            Log.w(TAG, "WebSocket handshake error", e)
            return
        }

        onNewConnection()
        val splitter = AnnexBNalSplitter()
        val input = socket.getInputStream()
        try {
            while (running.get()) {
                val payload = readFrame(input) ?: break
                val arrivalNanos = System.nanoTime()
                for (nal in splitter.push(payload)) {
                    stats.onNalArrival(arrivalNanos, nal.size)
                    onNal(nal)
                }
            }
        } catch (e: Exception) {
            if (running.get()) Log.w(TAG, "WebSocket read error", e)
        }

        try {
            socket.close()
        } catch (e: Exception) { /* already closing */ }
        Log.i(TAG, "Relay disconnected")
    }

    /** Reads the HTTP Upgrade request line-by-line and replies with the RFC 6455 handshake response. */
    private fun performHandshake(socket: Socket): Boolean {
        val input = socket.getInputStream()
        val output = socket.getOutputStream()

        var webSocketKey: String? = null
        val lineBuf = StringBuilder()
        while (true) {
            val b = input.read()
            if (b < 0) return false
            if (b == '\r'.code) continue
            if (b == '\n'.code) {
                val line = lineBuf.toString()
                lineBuf.clear()
                if (line.isEmpty()) break // blank line ends the HTTP header block
                val prefix = "Sec-WebSocket-Key:"
                if (line.startsWith(prefix, ignoreCase = true)) {
                    webSocketKey = line.substring(prefix.length).trim()
                }
            } else {
                lineBuf.append(b.toChar())
            }
        }

        val key = webSocketKey ?: return false
        val accept = computeAcceptKey(key)
        val response = "HTTP/1.1 101 Switching Protocols\r\n" +
            "Upgrade: websocket\r\n" +
            "Connection: Upgrade\r\n" +
            "Sec-WebSocket-Accept: $accept\r\n" +
            "\r\n"
        output.write(response.toByteArray(Charsets.US_ASCII))
        output.flush()
        return true
    }

    private fun computeAcceptKey(clientKey: String): String {
        // Fixed magic string from RFC 6455 section 1.3 -- not a secret, just
        // a protocol constant every compliant WebSocket implementation uses.
        val magic = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        val sha1 = MessageDigest.getInstance("SHA-1")
        val hash = sha1.digest((clientKey + magic).toByteArray(Charsets.US_ASCII))
        return Base64.encodeToString(hash, Base64.NO_WRAP)
    }

    /**
     * Reads one WebSocket frame and returns its payload if it's a binary or
     * continuation... frame (this project's relay never sends fragmented
     * messages, so only whole single-frame binary messages are handled);
     * returns null on a close frame or stream end. Ping/pong/text frames
     * are drained and skipped since the relay never sends anything else.
     */
    private fun readFrame(input: InputStream): ByteArray? {
        while (true) {
            val b0 = input.read()
            if (b0 < 0) return null
            val b1 = input.read()
            if (b1 < 0) return null

            val opcode = b0 and 0x0F
            val masked = (b1 and 0x80) != 0
            var length = (b1 and 0x7F).toLong()

            if (length == 126L) {
                length = (readExact(input, 2)?.let { ((it[0].toInt() and 0xFF) shl 8) or (it[1].toInt() and 0xFF) } ?: return null).toLong()
            } else if (length == 127L) {
                val bytes = readExact(input, 8) ?: return null
                length = 0
                for (byte in bytes) length = (length shl 8) or (byte.toLong() and 0xFF)
            }

            val maskKey = if (masked) readExact(input, 4) ?: return null else null
            val payload = readExact(input, length.toInt()) ?: return null
            if (maskKey != null) {
                for (i in payload.indices) {
                    payload[i] = (payload[i].toInt() xor maskKey[i % 4].toInt()).toByte()
                }
            }

            when (opcode) {
                0x8 -> return null // close frame
                0x2 -> return payload // binary frame -- what tcp-ws-relay sends
                0x1, 0x9, 0xA -> continue // text/ping/pong -- not used by the relay, skip and read the next frame
                else -> continue
            }
        }
    }

    private fun readExact(input: InputStream, n: Int): ByteArray? {
        if (n == 0) return ByteArray(0)
        val buf = ByteArray(n)
        var offset = 0
        while (offset < n) {
            val read = input.read(buf, offset, n - offset)
            if (read < 0) return null
            offset += read
        }
        return buf
    }
}
