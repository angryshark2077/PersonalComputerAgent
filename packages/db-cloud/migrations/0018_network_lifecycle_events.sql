ALTER TABLE system_events
    DROP CONSTRAINT IF EXISTS system_events_event_type_check,
    DROP CONSTRAINT IF EXISTS system_events_source_check;

ALTER TABLE system_events
    ADD CONSTRAINT system_events_event_type_check CHECK (
        event_type IN (
            'system.metric_sampled',
            'system.health_changed',
            'collector.status_changed',
            'agent.started',
            'agent.stopped',
            'agent.crash_recovered',
            'system.sleep',
            'system.wake',
            'network.offline',
            'network.online'
        )
    ),
    ADD CONSTRAINT system_events_source_check CHECK (
        (event_type IN ('system.metric_sampled', 'system.health_changed') AND source = 'system')
        OR (event_type = 'collector.status_changed' AND source = 'collector.registry')
        OR (
            event_type IN (
                'agent.started',
                'agent.stopped',
                'agent.crash_recovered',
                'system.sleep',
                'system.wake',
                'network.offline',
                'network.online'
            )
            AND source = 'runtime.lifecycle'
        )
    );
