package com.example.latencypoc.tv

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.util.Log
import android.view.Surface
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

private const val TAG = "VideoDecoder"

/**
 * Feeds Annex-B NAL units (as parsed out by [AnnexBNalReader]) into a
 * hardware `MediaCodec` decoder, rendering straight to a `Surface`.
 *
 * Auto-detects H.264 vs HEVC from the wire instead of needing a rebuild to
 * switch -- the sender's codec choice (see send_stream.py's --codec flag)
 * is inferred purely from the NAL types actually seen: an HEVC VPS (type
 * 32, which H.264 has no equivalent of at all) means HEVC; an H.264 SPS
 * (type 7, a value HEVC's 6-bit NAL type field maps to something else
 * entirely -- HEVC type 7 is a slice segment) means H.264. Detection latches
 * on the very first recognized parameter-set NAL and never changes for the
 * life of one connection.
 *
 * There's no SDP/RTSP session here to carry codec-specific-data up front, so
 * this decoder isn't configured until it has actually seen a full parameter
 * set (SPS+PPS for H.264; VPS+SPS+PPS for HEVC) on the wire -- which the
 * sender re-sends in front of every keyframe (via ffmpeg's
 * `dump_extra=freq=keyframe` bitstream filter) specifically so a receiver
 * that joins mid-stream, like this one, can bootstrap from the next
 * keyframe without ever needing them out-of-band.
 *
 * Does NOT parse SPS for width/height (nontrivial for HEVC in particular --
 * short-term/long-term reference picture sets, sub-layer ordering info,
 * etc. all precede the dimension fields, and not worth reimplementing for a
 * POC). A fixed default is used for `MediaFormat`'s initial width/height
 * instead; `MediaCodec` renders at the stream's actual resolution
 * regardless once csd-0/1/2 are parsed internally.
 */
class VideoDecoder(
    private val surface: Surface,
    private val stats: StreamStats,
    private val onFirstFrameDecoded: () -> Unit,
) {
    private enum class Codec { H264, HEVC }

    private var detectedCodec: Codec? = null
    private var codec: MediaCodec? = null
    private var configured = false

    // H.264 parameter sets.
    private var h264Sps: ByteArray? = null
    private var h264Pps: ByteArray? = null

    // HEVC parameter sets.
    private var hevcVps: ByteArray? = null
    private var hevcSps: ByteArray? = null
    private var hevcPps: ByteArray? = null

    private var firstFrameSeen = false

    // Consecutive NALs sharing one access unit are buffered together and
    // submitted as a single input buffer only once a NAL belonging to the
    // NEXT access unit shows up -- both codecs can spread one frame's worth
    // of data (parameter sets + slice segments) across several NALs, and
    // MediaCodec expects one queueInputBuffer call per access unit, not per
    // NAL.
    private val accessUnit = ByteArrayOutputStream()
    private var accessUnitHasSlice = false

    private val pendingUnits = LinkedBlockingQueue<ByteArray>()
    @Volatile private var running = false
    private var feederThread: Thread? = null

    fun start() {
        running = true
        feederThread = Thread({ feedLoop() }, "decoder-feeder").apply { start() }
    }

    fun stop() {
        running = false
        feederThread?.interrupt()
        feederThread?.join(1000)
        feederThread = null
        releaseCodecAndState()
        pendingUnits.clear()
    }

    /** Called from the TCP receiver thread; hands off to the feeder thread via queue. */
    fun onNal(nal: ByteArray) {
        pendingUnits.offer(nal)
    }

    /**
     * Called from the TCP receiver thread when a new connection is
     * accepted, before any of its NALs arrive. A restarted sender might
     * switch codecs (see send_stream.py's --codec flag), so codec
     * detection and any decoder built from the PREVIOUS connection's
     * parameter sets must not carry over -- otherwise the new stream's NALs
     * would be interpreted against a stale (and possibly wrong-codec)
     * MediaCodec instance. Offered as a queued marker rather than mutating
     * state directly from this thread, so it's applied in order relative to
     * whatever NALs are still in flight from the old connection.
     */
    fun onNewConnection() {
        pendingUnits.offer(RESET_MARKER)
    }

    /** Runs on the feeder thread -- tears down any decoder from the previous connection and clears detection state. */
    private fun releaseCodecAndState() {
        try {
            codec?.stop()
            codec?.release()
        } catch (e: Exception) {
            Log.w(TAG, "error releasing codec", e)
        }
        codec = null
        configured = false
        detectedCodec = null
        h264Sps = null
        h264Pps = null
        hevcVps = null
        hevcSps = null
        hevcPps = null
        firstFrameSeen = false
        accessUnit.reset()
        accessUnitHasSlice = false
    }

    private fun feedLoop() {
        while (running) {
            val nal = try {
                pendingUnits.poll(200, TimeUnit.MILLISECONDS) ?: continue
            } catch (e: InterruptedException) {
                break
            }
            if (nal === RESET_MARKER) {
                releaseCodecAndState()
                continue
            }
            if (nal.isEmpty()) continue

            if (detectedCodec == null) {
                detectedCodec = detectCodec(nal) ?: continue // not a recognizable parameter-set NAL yet
                Log.i(TAG, "Detected codec: $detectedCodec")
            }
            val activeCodec = detectedCodec ?: continue

            val isSlice = when (activeCodec) {
                Codec.H264 -> handleH264ParameterSet(nal)
                Codec.HEVC -> handleHevcParameterSet(nal)
            }

            if (!configured) {
                if (!tryConfigureCodec(activeCodec)) continue // still waiting for the full parameter set
            }

            // A non-slice NAL (parameter set/SEI/AUD) arriving after the
            // current access unit already has slice data means that access
            // unit is complete and this NAL belongs to the next one --
            // flush before accumulating further.
            if (!isSlice && accessUnitHasSlice) {
                flushAccessUnit()
            }
            accessUnit.write(START_CODE)
            accessUnit.write(nal)
            if (isSlice) accessUnitHasSlice = true
        }
    }

    /** H.264: SPS=7, PPS=8; NAL type is bits [0..4] of the single-byte header. Returns true if this NAL is a slice. */
    private fun handleH264ParameterSet(nal: ByteArray): Boolean {
        val nalType = nal[0].toInt() and 0x1F
        when (nalType) {
            7 -> h264Sps = nal
            8 -> h264Pps = nal
        }
        return nalType in 1..5
    }

    /** HEVC: VPS=32, SPS=33, PPS=34; NAL type is bits [1..6] of the two-byte header. Returns true if this NAL is a slice. */
    private fun handleHevcParameterSet(nal: ByteArray): Boolean {
        val nalType = (nal[0].toInt() and 0x7E) shr 1
        when (nalType) {
            32 -> hevcVps = nal
            33 -> hevcSps = nal
            34 -> hevcPps = nal
        }
        return nalType <= 21
    }

    /** Identifies which codec a stream is from its first parameter-set NAL, or null if `nal` isn't one. */
    private fun detectCodec(nal: ByteArray): Codec? {
        val h264Type = nal[0].toInt() and 0x1F
        if (h264Type == 7 || h264Type == 8) return Codec.H264

        val hevcType = (nal[0].toInt() and 0x7E) shr 1
        if (hevcType == 32 || hevcType == 33 || hevcType == 34) return Codec.HEVC

        return null
    }

    private fun tryConfigureCodec(activeCodec: Codec): Boolean {
        return when (activeCodec) {
            Codec.H264 -> {
                val s = h264Sps
                val p = h264Pps
                if (s == null || p == null) return false
                configureH264(s, p)
                true
            }
            Codec.HEVC -> {
                val v = hevcVps
                val s = hevcSps
                val p = hevcPps
                if (v == null || s == null || p == null) return false
                configureHevc(v, s, p)
                true
            }
        }
    }

    private fun configureH264(spsBytes: ByteArray, ppsBytes: ByteArray) {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, DEFAULT_WIDTH, DEFAULT_HEIGHT).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(annexB(spsBytes)))
            setByteBuffer("csd-1", ByteBuffer.wrap(annexB(ppsBytes)))
            applyColorAndPriorityHints()
        }
        startCodec(MediaFormat.MIMETYPE_VIDEO_AVC, format)
    }

    private fun configureHevc(vpsBytes: ByteArray, spsBytes: ByteArray, ppsBytes: ByteArray) {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_HEVC, DEFAULT_WIDTH, DEFAULT_HEIGHT).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(annexB(vpsBytes) + annexB(spsBytes) + annexB(ppsBytes)))
            applyColorAndPriorityHints()
        }
        startCodec(MediaFormat.MIMETYPE_VIDEO_HEVC, format)
    }

    private fun MediaFormat.applyColorAndPriorityHints() {
        setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
        setInteger(MediaFormat.KEY_PRIORITY, 0)
        // Matches the VUI the sender patches into the bitstream itself (see
        // send_stream.py's *_metadata bsf) -- ddagrab's capture is
        // full-range BT.709, and MediaCodec otherwise defaults to assuming
        // limited range, which produces a washed-out/desaturated picture.
        // Redundant with the bitstream's own VUI in principle, but costs
        // nothing to be explicit, and some OEM decoder stacks are
        // inconsistent about honoring VUI alone.
        setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_FULL)
        setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
        setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
    }

    private fun startCodec(mimeType: String, format: MediaFormat) {
        try {
            val mc = MediaCodec.createDecoderByType(mimeType)
            mc.configure(format, surface, null, 0)
            mc.start()
            codec = mc
            configured = true
            Log.i(TAG, "$mimeType decoder configured")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to configure decoder for $mimeType", e)
        }
    }

    private fun flushAccessUnit() {
        if (accessUnit.size() > 0) {
            submitToCodec(accessUnit.toByteArray())
        }
        accessUnit.reset()
        accessUnitHasSlice = false
    }

    private fun submitToCodec(accessUnitBytes: ByteArray) {
        val mc = codec ?: return
        val submitNanos = System.nanoTime()
        try {
            val inputIndex = mc.dequeueInputBuffer(10_000)
            if (inputIndex < 0) return
            val inputBuffer = mc.getInputBuffer(inputIndex) ?: return
            inputBuffer.clear()
            inputBuffer.put(accessUnitBytes)
            // presentationTimeUs just needs to be monotonically non-decreasing
            // for the codec's own reordering -- there's no timestamp at all
            // on this raw TCP connection, so a simple wall-clock-derived
            // counter is used instead. Absolute value doesn't matter for a
            // live, non-seekable stream like this.
            val ptsUs = submitNanos / 1000L
            mc.queueInputBuffer(inputIndex, 0, accessUnitBytes.size, ptsUs, 0)

            drainOutput(submitNanos)
        } catch (e: Exception) {
            Log.w(TAG, "codec submit/drain error", e)
        }
    }

    private fun drainOutput(submitNanos: Long) {
        val mc = codec ?: return
        val info = MediaCodec.BufferInfo()
        while (true) {
            val outputIndex = mc.dequeueOutputBuffer(info, 0)
            if (outputIndex < 0) break

            val decodeLatencyMs = (System.nanoTime() - submitNanos) / 1_000_000.0
            stats.lastDecodeLatencyMs = decodeLatencyMs
            stats.framesDecoded.incrementAndGet()

            mc.releaseOutputBuffer(outputIndex, true) // true = render to the Surface immediately
            stats.framesRendered.incrementAndGet()

            if (!firstFrameSeen) {
                firstFrameSeen = true
                onFirstFrameDecoded()
            }
        }
    }

    companion object {
        private val START_CODE = byteArrayOf(0, 0, 0, 1)
        private const val DEFAULT_WIDTH = 1920
        private const val DEFAULT_HEIGHT = 1080

        // Sentinel instance (identity-compared with ===) queued alongside
        // real NALs to signal "a new connection just started" in order,
        // without needing a second queue or extra synchronization between
        // the TCP receiver thread and the feeder thread.
        private val RESET_MARKER = ByteArray(0)

        private fun annexB(nal: ByteArray): ByteArray = START_CODE + nal
    }
}
