package com.savvagent.ottoflags.example;

import com.savvagent.ottoflags.sdk.OttoFlagsClient;
import com.savvagent.ottoflags.sdk.OttoFlagsConfig;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

@Configuration
public class OttoFlagsConfig {

    @Value("${ottoFlags.api-url}")
    private String apiUrl;

    @Value("${ottoFlags.sdk-key}")
    private String sdkKey;

    @Value("${ottoFlags.environment}")
    private String environment;

    @Bean
    public OttoFlagsClient ottoFlagsClient() {
        return new OttoFlagsClient(
            OttoFlagsConfig.builder()
                .apiUrl(apiUrl)
                .sdkKey(sdkKey)
                .environment(environment)
                .cacheEnabled(true)
                .cacheTtl(60000) // 1 minute
                .build()
        );
    }
}
