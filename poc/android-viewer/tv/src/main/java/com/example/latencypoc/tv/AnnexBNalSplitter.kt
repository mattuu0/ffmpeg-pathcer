package com.example.latencypoc.tv

import java.io.ByteArrayOutputStream

/**
 * Splits a raw Annex-B byte stream (0x000001 / 0x00000001 start codes) back
 * into individual NAL units, fed by pushing arbitrarily-sized chunks in as
 * they arrive rather than pulling from a blocking `InputStream`.
 *
 * Used by both receive paths this project supports: a plain TCP socket
 * (see TcpReceiver, which pushes each InputStream.read() result) and
 * WebSocket binary frames (see WebSocketReceiver, which pushes each frame's
 * payload). Neither path aligns its chunk boundaries to NAL boundaries --
 * ffmpeg's own TCP writes don't, and the relay (see
 * poc/latency-poc/tcp-ws-relay) deliberately forwards whatever a single TCP
 * read produced as one WebSocket frame without re-chunking -- so this
 * treats the input as one continuous byte stream and finds NAL boundaries
 * itself via start codes, same as the original TCP-only version did.
 */
class AnnexBNalSplitter {
    private val pending = ByteArrayOutputStream()

    /**
     * Feeds one chunk of bytes (a socket read, a WebSocket frame payload,
     * etc.) and returns every complete NAL unit (start code stripped) that
     * became available as a result -- zero, one, or several, since one
     * chunk can complete more than one NAL if it was buffered for a while.
     */
    fun push(chunk: ByteArray): List<ByteArray> {
        pending.write(chunk)
        val completed = mutableListOf<ByteArray>()
        while (true) {
            val nal = extractCompleteNal() ?: break
            completed.add(nal)
        }
        return completed
    }

    fun reset() {
        pending.reset()
    }

    /** Pulls one complete NAL out of `pending` if a second start code has arrived, else null. */
    private fun extractCompleteNal(): ByteArray? {
        val buf = pending.toByteArray()
        val firstStart = findStartCode(buf, 0) ?: return null
        val secondStart = findStartCode(buf, firstStart.end) ?: return null

        val nal = buf.copyOfRange(firstStart.end, secondStart.start)
        // Keep everything from the second start code onward -- it's the
        // beginning of the NAL still being accumulated.
        pending.reset()
        pending.write(buf, secondStart.start, buf.size - secondStart.start)
        return nal
    }

    private data class StartCode(val start: Int, val end: Int)

    private fun findStartCode(data: ByteArray, from: Int): StartCode? {
        var i = from
        while (i + 2 < data.size) {
            if (data[i] == 0.toByte() && data[i + 1] == 0.toByte()) {
                if (data[i + 2] == 1.toByte()) return StartCode(i, i + 3)
                if (i + 3 < data.size && data[i + 2] == 0.toByte() && data[i + 3] == 1.toByte()) {
                    return StartCode(i, i + 4)
                }
            }
            i++
        }
        return null
    }
}
