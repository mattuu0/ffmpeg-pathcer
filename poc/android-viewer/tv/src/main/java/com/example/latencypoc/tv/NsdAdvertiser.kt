package com.example.latencypoc.tv

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log

private const val TAG = "NsdAdvertiser"

/**
 * Advertises this receiver over mDNS/NSD so the Windows sender can find its
 * IPv4 address without the user having to look it up manually (e.g. in the
 * Fire TV network settings screen). The Windows side resolves this via
 * zeroconf, matching on [SERVICE_TYPE] and [INSTANCE_NAME].
 *
 * There is exactly one receiver instance expected on the LAN for this POC,
 * so a fixed instance name is fine -- NsdManager would otherwise silently
 * rename it (e.g. "name (2)") on a collision, which the sender doesn't need
 * to handle since it only ever looks for [INSTANCE_NAME]'s prefix.
 */
class NsdAdvertiser(private val context: Context) {
    private val nsdManager: NsdManager by lazy {
        context.getSystemService(Context.NSD_SERVICE) as NsdManager
    }
    private var registrationListener: NsdManager.RegistrationListener? = null

    fun start(port: Int) {
        stop()

        val serviceInfo = NsdServiceInfo().apply {
            serviceName = INSTANCE_NAME
            serviceType = SERVICE_TYPE
            setPort(port)
        }

        val listener = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(info: NsdServiceInfo) {
                Log.i(TAG, "NSD service registered: ${info.serviceName}")
            }
            override fun onRegistrationFailed(info: NsdServiceInfo, errorCode: Int) {
                Log.w(TAG, "NSD registration failed (errorCode=$errorCode)")
            }
            override fun onServiceUnregistered(info: NsdServiceInfo) {
                Log.i(TAG, "NSD service unregistered")
            }
            override fun onUnregistrationFailed(info: NsdServiceInfo, errorCode: Int) {
                Log.w(TAG, "NSD unregistration failed (errorCode=$errorCode)")
            }
        }

        try {
            nsdManager.registerService(serviceInfo, NsdManager.PROTOCOL_DNS_SD, listener)
            registrationListener = listener
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start NSD registration", e)
        }
    }

    fun stop() {
        val listener = registrationListener ?: return
        try {
            nsdManager.unregisterService(listener)
        } catch (e: Exception) {
            Log.w(TAG, "Error unregistering NSD service", e)
        }
        registrationListener = null
    }

    companion object {
        // Custom service type (not a registered IANA one) -- fine for a LAN-only
        // POC where both ends are code controlled by this project.
        const val SERVICE_TYPE = "_latencypoc._udp."
        const val INSTANCE_NAME = "latencypoc-viewer"
    }
}
