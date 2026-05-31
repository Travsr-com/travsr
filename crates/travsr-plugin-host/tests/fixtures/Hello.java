package com.example;

import java.util.List;
import java.util.ArrayList;

public class PaymentService {
    private final String apiKey;

    public PaymentService(String apiKey) {
        this.apiKey = apiKey;
    }

    public boolean charge(double amount) {
        return apiKey != null && !apiKey.isEmpty() && amount > 0;
    }

    public List<String> getTransactions() {
        return new ArrayList<>();
    }
}
