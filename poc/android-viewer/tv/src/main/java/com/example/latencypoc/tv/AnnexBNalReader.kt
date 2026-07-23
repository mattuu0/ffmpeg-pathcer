package com.example.latencypoc.tv

import java.io.ByteArrayOutputStream
import java.io.InputStream

/**
 * Splits a raw Annex-B byte stream (0x000001 / 0x00000001 start codes) read
 * from a plain TCP socket back into individual NAL units.
 *
 * There is no RTP framing on this connection at all -- the Windows sender
 * just writes ffmpeg's `-f hevc` raw output directly to the socket, so the
 * only structure in the byte stream is the Annex-B start codes HEVC (like
 * H.264) delimits NAL units with. TCP's own ordering/retransmission means
 * this never has to handle out-of-order or missing bytes -- unlike the
 * RTP/UDP version this replaced, a dropped segment here would just stall the
 * read (TCP retransmits under the hood) rather than silently producing a
 * torn NAL.
 */
class AnnexBNalReader(private val input: InputStream) {
    private val readBuf = ByteArray(64 * 1024)
    private val pending = ByteArrayOutputStream()

    /**
     * Blocks until one full NAL unit (start code stripped) is available, or
     * returns null on end-of-stream/error. Whatever bytes arrive from a
     * single `InputStream.read()` are buffered until a subsequent start
     * code shows the previous NAL is complete -- exactly one NAL is handed
     * back per call, oldest first.
     */
    fun readNextNal(): ByteArray? {
        while (true) {
            val extracted = extractCompleteNal()
            if (extracted != null) return extracted

            val n = try {
                input.read(readBuf)
            } catch (e: Exception) {
                return null
            }
            if (n < 0) {
                // Stream closed -- flush whatever's left as a final NAL, if any.
                return flushRemaining()
            }
            pending.write(readBuf, 0, n)
        }
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

    private fun flushRemaining(): ByteArray? {
        val buf = pending.toByteArray()
        pending.reset()
        val start = findStartCode(buf, 0) ?: return null
        if (start.end >= buf.size) return null
        return buf.copyOfRange(start.end, buf.size)
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
