package ai.diffforge.haider.daemon

import android.os.Parcel
import android.os.Parcelable

/** C2 field order is a wire contract. Nullable fields have an explicit 0/1 presence word. */
data class RpcEndpoint(val path: String, val wireProtocol: Int, val daemonGeneration: Long) : Parcelable {
    override fun writeToParcel(out: Parcel, flags: Int) {
        out.writeString(path)
        out.writeInt(wireProtocol)
        out.writeLong(daemonGeneration)
    }
    override fun describeContents() = 0
    companion object {
        @JvmField val CREATOR = object : Parcelable.Creator<RpcEndpoint> {
            override fun createFromParcel(input: Parcel) = RpcEndpoint(
                requireNotNull(input.readString()), input.readInt(), input.readLong(),
            )
            override fun newArray(size: Int) = arrayOfNulls<RpcEndpoint>(size)
        }
    }
}

data class DaemonServiceSnapshot(
    val enabled: Boolean = false,
    val phase: String = "DISABLED",
    val appVersion: String = "",
    val nativeVersion: String = "",
    val wireProtocol: Int = 0,
    val daemonGeneration: Long = 0,
    val rpcEndpoint: RpcEndpoint? = null,
    val restartAttempt: Int = 0,
    val nextRetryUnixMs: Long? = null,
    val network: String = "UNKNOWN",
    val notificationsGranted: Boolean = false,
    val batteryRestricted: Boolean = false,
    val errorCode: String? = null,
    val errorRetryable: Boolean = false,
    val snapshotSeq: Long = 0,
    val startedAtElapsedRealtimeMs: Long? = null,
    val pssBytes: Long? = null,
) : Parcelable {
    override fun writeToParcel(out: Parcel, flags: Int) {
        out.writeInt(if (enabled) 1 else 0)
        out.writeString(phase)
        out.writeString(appVersion)
        out.writeString(nativeVersion)
        out.writeInt(wireProtocol)
        out.writeLong(daemonGeneration)
        out.writeInt(if (rpcEndpoint == null) 0 else 1)
        rpcEndpoint?.writeToParcel(out, flags)
        out.writeInt(restartAttempt)
        out.writeNullableLong(nextRetryUnixMs)
        out.writeString(network)
        out.writeInt(if (notificationsGranted) 1 else 0)
        out.writeInt(if (batteryRestricted) 1 else 0)
        out.writeInt(if (errorCode == null) 0 else 1)
        errorCode?.let(out::writeString)
        out.writeInt(if (errorRetryable) 1 else 0)
        out.writeLong(snapshotSeq)
        out.writeNullableLong(startedAtElapsedRealtimeMs)
        out.writeNullableLong(pssBytes)
    }
    override fun describeContents() = 0
    companion object {
        @JvmField val CREATOR = object : Parcelable.Creator<DaemonServiceSnapshot> {
            override fun createFromParcel(input: Parcel) = DaemonServiceSnapshot(
                enabled = input.readInt() != 0,
                phase = requireNotNull(input.readString()),
                appVersion = requireNotNull(input.readString()),
                nativeVersion = requireNotNull(input.readString()),
                wireProtocol = input.readInt(),
                daemonGeneration = input.readLong(),
                rpcEndpoint = if (input.readInt() == 0) null else RpcEndpoint.CREATOR.createFromParcel(input),
                restartAttempt = input.readInt(),
                nextRetryUnixMs = input.readNullableLong(),
                network = requireNotNull(input.readString()),
                notificationsGranted = input.readInt() != 0,
                batteryRestricted = input.readInt() != 0,
                errorCode = if (input.readInt() == 0) null else requireNotNull(input.readString()),
                errorRetryable = input.readInt() != 0,
                snapshotSeq = input.readLong(),
                startedAtElapsedRealtimeMs = input.readNullableLong(),
                pssBytes = input.readNullableLong(),
            )
            override fun newArray(size: Int) = arrayOfNulls<DaemonServiceSnapshot>(size)
        }
    }
}

private fun Parcel.writeNullableLong(value: Long?) {
    writeInt(if (value == null) 0 else 1)
    value?.let(::writeLong)
}
private fun Parcel.readNullableLong(): Long? = if (readInt() == 0) null else readLong()
