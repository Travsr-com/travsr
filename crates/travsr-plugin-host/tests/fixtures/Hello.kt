package com.example

import kotlin.collections.List

class PaymentService(private val apiKey: String) {

    fun charge(amount: Double): Boolean {
        return apiKey.isNotEmpty() && amount > 0
    }

    fun getTransactions(): List<String> = emptyList()
}

fun processPayment(service: PaymentService, amount: Double) = service.charge(amount)
