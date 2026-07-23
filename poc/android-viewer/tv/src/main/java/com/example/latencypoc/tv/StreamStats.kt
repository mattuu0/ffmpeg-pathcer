package com.example.latencypoc.tv

import java.util.concurrent.atomic.AtomicLong

/**
 * Thread-safe rolling counters shared between the network receive thread,
 * the MediaCodec decode callback thread, and the Compose UI recomposition
 * -- each updates a subset of these, and the UI just samples them on a
 * timer rather than needing its own synchronization.
 */
class StreamStats {
    val bytesReceived = AtomicLong(0)
    val nalsReceived = AtomicLong(0)
    val framesDecoded = AtomicLong(0)
    val framesRendered = AtomicLong(0)

    @Volatile var lastNalIntervalMs: Double = 0.0
    @Volatile var lastDecodeLatencyMs: Double = 0.0
    @Volatile var currentBitrateKbps: Double = 0.0
    @Volatile var currentFps: Double = 0.0
    @Volatile var connected: Boolean = false

    private var lastNalArrivalNanos: Long? = null

    /**
     * There's no RTP timestamp on this connection to compare against (see
     * TcpReceiver's doc comment for why TCP replaced RTP/UDP here), so this
     * just tracks wall-clock spacing between consecutive NALs arriving on
     * the socket -- a rough proxy for how bursty/smooth delivery is, not a
     * true one-way network latency (that would need clock sync between the
     * two devices, out of scope for this POC).
     */
    fun onNalArrival(arrivalNanos: Long, byteCount: Int) {
        bytesReceived.addAndGet(byteCount.toLong())
        nalsReceived.incrementAndGet()

        val last = lastNalArrivalNanos
        if (last != null) {
            lastNalIntervalMs = (arrivalNanos - last) / 1_000_000.0
        }
        lastNalArrivalNanos = arrivalNanos
    }

    fun reset() {
        bytesReceived.set(0)
        nalsReceived.set(0)
        framesDecoded.set(0)
        framesRendered.set(0)
        lastNalArrivalNanos = null
        lastNalIntervalMs = 0.0
        lastDecodeLatencyMs = 0.0
        currentBitrateKbps = 0.0
        currentFps = 0.0
        connected = false
    }
}
