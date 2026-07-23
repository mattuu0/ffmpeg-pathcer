package com.example.latencypoc.tv

import android.util.Log
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

private const val TAG = "TcpReceiver"

/**
 * Listens (server role) on a fixed TCP port for the Windows sender to
 * connect, then reads its raw Annex-B byte stream and feeds NAL units to
 * [VideoDecoder] as they're parsed out.
 *
 * The Android side listens rather than connects so it can be started once
 * and left running -- the Windows sender can be (re)started any number of
 * times against the same listening socket, accepting a fresh TCP connection
 * each time, rather than the receiver needing to already know the sender's
 * address up front (it still needs to be discoverED via mDNS the other way
 * around -- this is purely about which side calls connect() vs accept()).
 *
 * [onNewConnection] fires once per accepted connection, before any of its
 * NALs are handed to [onNal] -- the caller uses it to reset [VideoDecoder]'s
 * codec detection, since a restarted sender might switch between H.264 and
 * HEVC (see send_stream.py's --codec flag) and the old connection's
 * detected codec must not leak into the new one.
 */
class TcpReceiver(
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
        thread = Thread({ runLoop(boundLatch) }, "tcp-receiver").apply { start() }
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
            Log.i(TAG, "Listening for TCP connections on port $port")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to bind TCP port $port", e)
            running.set(false)
            return
        } finally {
            boundLatch.countDown()
        }

        // Accepts connections in a loop rather than just once -- if the
        // Windows sender is stopped and restarted, this lets the same
        // running Android app pick up the new connection without needing
        // to be relaunched itself.
        while (running.get()) {
            val socket = try {
                serverSocket?.accept() ?: break
            } catch (e: Exception) {
                if (running.get()) Log.w(TAG, "accept() error", e)
                break
            }

            Log.i(TAG, "Sender connected from ${socket.remoteSocketAddress}")
            clientSocket = socket
            stats.connected = true
            onNewConnection()
            handleConnection(socket)
            stats.connected = false
            clientSocket = null
        }

        try {
            serverSocket?.close()
        } catch (e: Exception) { /* ignore */ }
        Log.i(TAG, "TCP receiver stopped")
    }

    private fun handleConnection(socket: Socket) {
        socket.tcpNoDelay = true
        val reader = AnnexBNalReader(socket.getInputStream())
        while (running.get()) {
            val nal = reader.readNextNal() ?: break
            val arrivalNanos = System.nanoTime()
            stats.onNalArrival(arrivalNanos, nal.size)
            onNal(nal)
        }
        try {
            socket.close()
        } catch (e: Exception) { /* already closing */ }
        Log.i(TAG, "Sender disconnected")
    }
}
