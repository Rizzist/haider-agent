package ai.diffforge.haider.daemon;
import ai.diffforge.haider.daemon.DaemonServiceSnapshot;
oneway interface IDaemonSnapshotListener {
    void onSnapshot(in DaemonServiceSnapshot snapshot);
}
