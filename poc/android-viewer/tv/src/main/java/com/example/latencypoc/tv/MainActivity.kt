package com.example.latencypoc.tv

import android.os.Bundle
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.tv.material3.MaterialTheme
import androidx.tv.material3.Text
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/** TCP port the Windows sender connects to. Fixed for this POC -- see poc/latency-poc README. */
private const val LISTEN_PORT = 5000
private const val STATS_REFRESH_MS = 500L

class MainActivity : ComponentActivity() {
    private val stats = StreamStats()
    private var receiver: TcpReceiver? = null
    private var decoder: VideoDecoder? = null
    private lateinit var nsdAdvertiser: NsdAdvertiser

    private var lastFrameCountForFps = 0L
    private var lastFpsSampleNanos = System.nanoTime()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // This is a viewer meant to sit on a TV/monitor for the length of a
        // whole streaming session -- letting the display sleep mid-session
        // would be far more disruptive here than the battery-life tradeoff
        // this flag normally exists to avoid on a handheld device.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        nsdAdvertiser = NsdAdvertiser(applicationContext)

        setContent {
            MaterialTheme {
                Box(modifier = Modifier.fillMaxSize().background(Color.Black)) {
                    ViewerScreen(
                        stats = stats,
                        onSurfaceReady = { surface -> startPipeline(surface) },
                        onSurfaceDestroyed = { stopPipeline() },
                    )
                }
            }
        }

        lifecycleScope.launch {
            while (true) {
                delay(STATS_REFRESH_MS)
                sampleFps()
            }
        }
    }

    private fun sampleFps() {
        val now = System.nanoTime()
        val frames = stats.framesRendered.get()
        val elapsedSec = (now - lastFpsSampleNanos) / 1_000_000_000.0
        if (elapsedSec > 0) {
            stats.currentFps = (frames - lastFrameCountForFps) / elapsedSec
        }
        lastFrameCountForFps = frames
        lastFpsSampleNanos = now

        val bytes = stats.bytesReceived.get()
        stats.currentBitrateKbps = bytes * 8.0 / 1000.0 / (elapsedSec.coerceAtLeast(0.001))
        stats.bytesReceived.set(0)
    }

    private fun startPipeline(surface: Surface) {
        stopPipeline()
        stats.reset()

        val dec = VideoDecoder(surface, stats) {}
        dec.start()
        decoder = dec

        val recv = TcpReceiver(
            LISTEN_PORT,
            onNewConnection = { decoder?.onNewConnection() },
            onNal = { nal -> decoder?.onNal(nal) },
            stats = stats,
        )
        recv.start()
        receiver = recv

        // Advertised only once the socket is actually bound and listening --
        // otherwise the Windows sender could discover this device via mDNS
        // and try to connect before TcpReceiver.start() has bound the port.
        nsdAdvertiser.start(LISTEN_PORT)
    }

    private fun stopPipeline() {
        nsdAdvertiser.stop()
        receiver?.stop()
        receiver = null
        decoder?.stop()
        decoder = null
    }

    override fun onDestroy() {
        stopPipeline()
        super.onDestroy()
    }
}

@Composable
private fun ViewerScreen(
    stats: StreamStats,
    onSurfaceReady: (Surface) -> Unit,
    onSurfaceDestroyed: () -> Unit,
) {
    Box(modifier = Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { context ->
                SurfaceView(context).apply {
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(holder: SurfaceHolder) {
                            onSurfaceReady(holder.surface)
                        }
                        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            onSurfaceDestroyed()
                        }
                    })
                }
            },
        )

        StatsOverlay(stats = stats, modifier = Modifier
            .align(Alignment.TopStart)
            .padding(24.dp))
    }
}

@Composable
private fun StatsOverlay(stats: StreamStats, modifier: Modifier = Modifier) {
    // Re-sampled on a timer rather than reacting to StreamStats mutation
    // directly (it isn't Compose State) -- polling every 500ms is more than
    // adequate for a human-readable diagnostic overlay, and avoids wiring up
    // synchronization between the network/decoder threads and Compose state.
    var connected by remember { mutableStateOf(false) }
    var fps by remember { mutableStateOf(0.0) }
    var bitrateKbps by remember { mutableStateOf(0.0) }
    var nalIntervalMs by remember { mutableStateOf(0.0) }
    var decodeLatencyMs by remember { mutableStateOf(0.0) }
    var nals by remember { mutableStateOf(0L) }
    var framesDecoded by remember { mutableStateOf(0L) }

    LaunchedEffect(Unit) {
        while (true) {
            connected = stats.connected
            fps = stats.currentFps
            bitrateKbps = stats.currentBitrateKbps
            nalIntervalMs = stats.lastNalIntervalMs
            decodeLatencyMs = stats.lastDecodeLatencyMs
            nals = stats.nalsReceived.get()
            framesDecoded = stats.framesDecoded.get()
            delay(STATS_REFRESH_MS)
        }
    }

    Column(
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.6f))
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        val statusColor = if (connected) Color(0xFF4CAF50) else Color(0xFFF44336)
        Text(
            text = if (connected) "● RECEIVING" else "○ WAITING FOR STREAM (TCP $LISTEN_PORT)",
            color = statusColor,
            fontSize = 18.sp,
        )
        Text(text = "FPS: %.1f".format(fps), color = Color.White, fontSize = 16.sp)
        Text(text = "Bitrate: %.0f kbps".format(bitrateKbps), color = Color.White, fontSize = 16.sp)
        Text(text = "NAL interval: %.1f ms".format(nalIntervalMs), color = Color.White, fontSize = 16.sp)
        Text(text = "Decode latency: %.1f ms".format(decodeLatencyMs), color = Color.White, fontSize = 16.sp)
        Text(text = "NALs: $nals  Frames: $framesDecoded", color = Color.Gray, fontSize = 14.sp)
    }
}
