package com.example.imespike.spike

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class KeystoreKekTest {

    @Before
    fun ensureKey() {
        KeystoreKek.generateKekIfAbsent()
    }

    @Test
    fun encrypt_decrypt_roundtrip_shortData() {
        val plaintext = "hello".toByteArray()
        val decrypted = KeystoreKek.decrypt(KeystoreKek.encrypt(plaintext))
        assertArrayEquals(plaintext, decrypted)
    }

    @Test
    fun encrypt_decrypt_roundtrip_longData() {
        val plaintext = (0..999).map { it.toByte() }.toByteArray()
        val decrypted = KeystoreKek.decrypt(KeystoreKek.encrypt(plaintext))
        assertArrayEquals(plaintext, decrypted)
    }

    @Test
    fun generateKekIfAbsent_idempotent() {
        KeystoreKek.generateKekIfAbsent()
        KeystoreKek.generateKekIfAbsent()
        val plaintext = "still works".toByteArray()
        assertArrayEquals(plaintext, KeystoreKek.decrypt(KeystoreKek.encrypt(plaintext)))
    }

    @Test
    fun encrypt_producesNonPlaintext() {
        val plaintext = "hello".toByteArray()
        val encrypted = KeystoreKek.encrypt(plaintext)
        assertFalse(encrypted.contentEquals(plaintext))
    }

    @Test
    fun encrypt_twiceSameInput_differentCiphertext() {
        val plaintext = "hello".toByteArray()
        val first = KeystoreKek.encrypt(plaintext)
        val second = KeystoreKek.encrypt(plaintext)
        assertFalse(first.contentEquals(second))
    }

    @Test(expected = Exception::class)
    fun decrypt_wrongData_throws() {
        val garbage = ByteArray(28) { it.toByte() } // 12 IV + 16 bytes, wrong tag
        KeystoreKek.decrypt(garbage)
    }

    @Test
    fun runSelfTest_returnsTrue() {
        assertTrue(KeystoreKek.runSelfTest("test pem data".toByteArray()))
    }
}
