package ai.diffforge.haider.daemon;
import ai.diffforge.haider.daemon.DaemonServiceSnapshot;
import ai.diffforge.haider.daemon.RpcEndpoint;
import ai.diffforge.haider.daemon.IDaemonSnapshotListener;
interface IHaiderDaemonService {
    DaemonServiceSnapshot getSnapshot();
    void registerListener(IDaemonSnapshotListener listener);
    void unregisterListener(IDaemonSnapshotListener listener);
    void startUserInitiated();
    void stopAndDisable();
    void restart();
    void prepareForUpdate();
    @nullable RpcEndpoint getRpcEndpoint();
}
